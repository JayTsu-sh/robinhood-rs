use async_trait::async_trait;
use bytes::Bytes;
use rbh_entry_store::model::{EntryKind, FileSystemId, ObjectId};
use tonic::transport::Channel;

use crate::juicefs_proto::changelog_client::ChangelogClient;
use crate::juicefs_proto::{AckRequest, ChangelogRecord, WatchRequest};
use crate::{Change, ChangeBatch, ChangeSource, ChangeSourceError, Checkpoint, CreatedMetadata};

pub struct JuiceFsChangeSource {
    filesystem: FileSystemId,
    volume: String,
    client: ChangelogClient<Channel>,
    stream: tonic::Streaming<ChangelogRecord>,
    pending: Option<Checkpoint>,
    last_acknowledged: u64,
}

impl JuiceFsChangeSource {
    pub async fn connect(
        filesystem: FileSystemId, endpoint: String, volume: String,
    ) -> Result<Self, ChangeSourceError> {
        let mut client = ChangelogClient::connect(endpoint).await?;
        let stream = client
            .watch(WatchRequest { volume: volume.clone() })
            .await
            .map_err(map_watch_status)?
            .into_inner();
        Ok(Self {
            filesystem,
            volume,
            client,
            stream,
            pending: None,
            last_acknowledged: 0,
        })
    }

    async fn reopen_watch(&mut self) -> Result<(), ChangeSourceError> {
        self.stream = self
            .client
            .watch(WatchRequest {
                volume: self.volume.clone(),
            })
            .await
            .map_err(map_watch_status)?
            .into_inner();
        Ok(())
    }
}

fn map_watch_status(status: tonic::Status) -> ChangeSourceError {
    if status.code() == tonic::Code::FailedPrecondition {
        ChangeSourceError::RetentionGap(status.message().to_owned())
    } else {
        ChangeSourceError::Rpc(status)
    }
}

#[async_trait]
impl ChangeSource for JuiceFsChangeSource {
    async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError> {
        if self.pending.is_some() {
            return Err(ChangeSourceError::PendingCheckpoint);
        }
        let record = loop {
            match self.stream.message().await {
                Ok(Some(record)) if record.version > 0 && record.version as u64 <= self.last_acknowledged => {
                    // The Agent cursor advances only after Ack, so reconnecting
                    // can legitimately replay the last durably applied record.
                    continue;
                }
                Ok(Some(record)) => break record,
                Ok(None) => self.reopen_watch().await?,
                Err(status) if status.code() == tonic::Code::Unavailable => self.reopen_watch().await?,
                Err(status) if status.code() == tonic::Code::FailedPrecondition => {
                    return Err(ChangeSourceError::RetentionGap(status.message().to_owned()));
                }
                Err(status) => return Err(status.into()),
            }
        };
        if record.volume != self.volume {
            return Err(ChangeSourceError::WrongVolume {
                expected: self.volume.clone(),
                actual: record.volume,
            });
        }
        if record.version <= 0 {
            return Err(ChangeSourceError::OutOfOrder {
                last: self.last_acknowledged,
                actual: record.version,
            });
        }
        let checkpoint = Checkpoint {
            source: self.volume.clone(),
            position: record.version as u64,
        };
        let change = parse_record(&record.entry)?;
        self.pending = Some(checkpoint.clone());
        Ok(Some(ChangeBatch {
            filesystem: self.filesystem.clone(),
            changes: change.into_iter().collect(),
            checkpoint,
        }))
    }

    async fn commit(&mut self, checkpoint: Checkpoint) -> Result<(), ChangeSourceError> {
        let expected = self.pending.as_ref().ok_or(ChangeSourceError::Closed)?;
        if expected != &checkpoint {
            return Err(ChangeSourceError::WrongSource {
                expected: format!("{}@{}", expected.source, expected.position),
                actual: format!("{}@{}", checkpoint.source, checkpoint.position),
            });
        }
        self.client
            .ack(AckRequest {
                volume: self.volume.clone(),
                version: checkpoint.position as i64,
            })
            .await?;
        self.last_acknowledged = checkpoint.position;
        self.pending = None;
        Ok(())
    }
}

fn parse_record(entry: &str) -> Result<Option<Change>, ChangeSourceError> {
    let (timestamp, rest) = entry
        .split_once('|')
        .ok_or_else(|| malformed(entry, "missing timestamp separator"))?;
    let seconds = timestamp
        .split_once('.')
        .map_or(timestamp, |(seconds, _)| seconds)
        .parse::<i64>()
        .map_err(|_| malformed(entry, "invalid timestamp"))?;
    let operation = rest.split_once('|').map_or(rest, |(operation, _)| operation);
    let open = operation
        .find('(')
        .ok_or_else(|| malformed(entry, "missing operation arguments"))?;
    let close = operation
        .rfind(')')
        .ok_or_else(|| malformed(entry, "unterminated operation arguments"))?;
    let name = &operation[..open];
    let fields: Vec<_> = operation[open + 1..close].split(',').collect();
    let result = operation[close + 1..].strip_prefix(':');
    match name {
        "CREATE" => {
            require_fields(entry, &fields, 10)?;
            let kind = match parse_u64(entry, fields[4], "entry type")? {
                1 => EntryKind::File,
                2 => EntryKind::Directory,
                3 => EntryKind::Symlink,
                4 => EntryKind::Fifo,
                5 => EntryKind::BlockDevice,
                6 => EntryKind::CharDevice,
                7 => EntryKind::Socket,
                value => return Err(malformed(entry, &format!("unknown entry type {value}"))),
            };
            Ok(Some(Change::Created {
                object: ObjectId::JuiceFs(parse_result(entry, result)?),
                parent: ObjectId::JuiceFs(parse_u64(entry, fields[0], "parent inode")?),
                name: Bytes::from(decode_name(entry, fields[1])?),
                kind,
                metadata: Some(CreatedMetadata {
                    uid: parse_u32(entry, fields[2], "uid")?,
                    gid: parse_u32(entry, fields[3], "gid")?,
                    mode: parse_u32(entry, fields[5], "mode")?,
                }),
                time: seconds,
            }))
        }
        "SETATTR" => {
            require_fields(entry, &fields, 14)?;
            Ok(Some(Change::MetadataChanged {
                object: ObjectId::JuiceFs(parse_u64(entry, fields[0], "inode")?),
                kind: crate::MetadataChangeKind::Attributes,
                time: seconds,
            }))
        }
        "SETXATTR" | "REMOVEXATTR" => {
            require_fields(entry, &fields, if name == "SETXATTR" { 4 } else { 2 })?;
            Ok(Some(Change::MetadataChanged {
                object: ObjectId::JuiceFs(parse_u64(entry, fields[0], "inode")?),
                kind: crate::MetadataChangeKind::Xattr,
                time: seconds,
            }))
        }
        "WRITE" => {
            require_fields(entry, &fields, 7)?;
            Ok(Some(Change::ContentChanged {
                object: ObjectId::JuiceFs(parse_u64(entry, fields[0], "inode")?),
                parent: None,
                name: Bytes::new(),
                kind: crate::ContentChangeKind::Data,
                time: seconds,
            }))
        }
        "MOVE" => {
            require_fields(entry, &fields, 7)?;
            Ok(Some(Change::Renamed {
                object: ObjectId::JuiceFs(parse_result(entry, result)?),
                source_parent: ObjectId::JuiceFs(parse_u64(entry, fields[0], "source parent")?),
                source_name: Bytes::from(decode_name(entry, fields[1])?),
                parent: ObjectId::JuiceFs(parse_u64(entry, fields[2], "destination parent")?),
                name: Bytes::from(decode_name(entry, fields[3])?),
                time: seconds,
            }))
        }
        "LINK" => {
            require_fields(entry, &fields, 4)?;
            Ok(Some(Change::Hardlinked {
                object: ObjectId::JuiceFs(parse_u64(entry, fields[0], "inode")?),
                parent: ObjectId::JuiceFs(parse_u64(entry, fields[1], "parent inode")?),
                name: Bytes::from(decode_name(entry, fields[2])?),
                time: seconds,
            }))
        }
        "UNLINK" | "RMDIR" => {
            require_fields(entry, &fields, if name == "UNLINK" { 5 } else { 3 })?;
            Ok(Some(Change::Removed {
                object: ObjectId::JuiceFs(parse_result(entry, result)?),
                parent: ObjectId::JuiceFs(parse_u64(entry, fields[0], "parent inode")?),
                name: Bytes::from(decode_name(entry, fields[1])?),
                // JuiceFS records identify the removed namespace edge but do not
                // expose the post-operation link count.
                last_link: name == "RMDIR",
                directory: name == "RMDIR",
                time: seconds,
            }))
        }
        // JuiceFS also logs counters, session maintenance, and other internal
        // metadata operations. They advance the Agent cursor but do not mutate
        // Robinhood's namespace catalog.
        "ACCESS"
        | "ATTACH"
        | "CLEANSESSION"
        | "CLEANUP"
        | "CLEANUP_DELAYED_SLICES"
        | "CLEANUP_TRASH_SLICES"
        | "DELCHUNK"
        | "DELETESLICE"
        | "DELETETOKENS"
        | "DELQUOTA"
        | "DELSUSTAINED"
        | "DIRSTAT"
        | "INCR_COUNTER"
        | "INIT_ENABLE_DIRSTATS"
        | "INIT_ENABLE_USERGROUPQUOTA"
        | "LOADDUMPEDACLS"
        | "NEWSESSION"
        | "SET"
        | "SETQUOTA"
        | "STORETOKEN"
        | "UPDATETOKEN" => Ok(None),
        _ => Err(malformed(entry, &format!("unsupported user operation {name}"))),
    }
}

fn require_fields(entry: &str, fields: &[&str], expected: usize) -> Result<(), ChangeSourceError> {
    if fields.len() == expected {
        Ok(())
    } else {
        Err(malformed(
            entry,
            &format!("expected {expected} fields, got {}", fields.len()),
        ))
    }
}

fn parse_result(entry: &str, result: Option<&str>) -> Result<u64, ChangeSourceError> {
    parse_u64(
        entry,
        result.ok_or_else(|| malformed(entry, "missing result inode"))?,
        "result inode",
    )
}

fn parse_u64(entry: &str, value: &str, field: &str) -> Result<u64, ChangeSourceError> {
    value.parse().map_err(|_| malformed(entry, &format!("invalid {field}")))
}

fn parse_u32(entry: &str, value: &str, field: &str) -> Result<u32, ChangeSourceError> {
    value.parse().map_err(|_| malformed(entry, &format!("invalid {field}")))
}

fn decode_name(entry: &str, encoded: &str) -> Result<Vec<u8>, ChangeSourceError> {
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| malformed(entry, "truncated escaped name"))?;
            let text = std::str::from_utf8(hex).map_err(|_| malformed(entry, "invalid escaped name"))?;
            decoded.push(u8::from_str_radix(text, 16).map_err(|_| malformed(entry, "invalid escaped name"))?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(decoded)
}

fn malformed(entry: &str, reason: &str) -> ChangeSourceError {
    ChangeSourceError::MalformedRecord(format!("{reason}: {entry}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentChangeKind, MetadataChangeKind};

    #[test]
    fn failed_precondition_is_a_retention_gap() {
        assert!(matches!(
            map_watch_status(tonic::Status::failed_precondition("changelog retention gap")),
            ChangeSourceError::RetentionGap(_)
        ));
    }

    #[test]
    fn normalizes_supported_juicefs_operations() {
        let cases = [
            (
                "1.0|SETATTR(42,1,0,1,1,420,0,1,1,0,0,1,0,0)|(1,2)",
                Change::MetadataChanged {
                    object: ObjectId::JuiceFs(42),
                    kind: MetadataChangeKind::Attributes,
                    time: 1,
                },
            ),
            (
                "2.0|WRITE(42,0,0,8,12,2,0):1|(1,3)",
                Change::ContentChanged {
                    object: ObjectId::JuiceFs(42),
                    parent: None,
                    name: Bytes::new(),
                    kind: ContentChangeKind::Data,
                    time: 2,
                },
            ),
            (
                "2.0|SETXATTR(42,user.tag,value,0)|(1,3)",
                Change::MetadataChanged {
                    object: ObjectId::JuiceFs(42),
                    kind: MetadataChangeKind::Xattr,
                    time: 2,
                },
            ),
            (
                "3.0|MOVE(1,old%2Cname,2,new%25name,0,0,0):42|(1,4)",
                Change::Renamed {
                    object: ObjectId::JuiceFs(42),
                    source_parent: ObjectId::JuiceFs(1),
                    source_name: Bytes::from_static(b"old,name"),
                    parent: ObjectId::JuiceFs(2),
                    name: Bytes::from_static(b"new%name"),
                    time: 3,
                },
            ),
            (
                "3.0|LINK(42,2,alias,true):2|(1,4)",
                Change::Hardlinked {
                    object: ObjectId::JuiceFs(42),
                    parent: ObjectId::JuiceFs(2),
                    name: Bytes::from_static(b"alias"),
                    time: 3,
                },
            ),
            (
                "4.0|UNLINK(2,new%25name,0,false,true):42|(1,5)",
                Change::Removed {
                    object: ObjectId::JuiceFs(42),
                    parent: ObjectId::JuiceFs(2),
                    name: Bytes::from_static(b"new%name"),
                    last_link: false,
                    directory: false,
                    time: 4,
                },
            ),
            (
                "5.0|RMDIR(1,empty,0):43|(1,6)",
                Change::Removed {
                    object: ObjectId::JuiceFs(43),
                    parent: ObjectId::JuiceFs(1),
                    name: Bytes::from_static(b"empty"),
                    last_link: true,
                    directory: true,
                    time: 5,
                },
            ),
        ];
        for (record, expected) in cases {
            assert_eq!(parse_record(record).unwrap(), Some(expected));
        }
    }

    #[test]
    fn rejects_malformed_and_unknown_records() {
        for record in [
            "bad",
            "1.0|SETATTR(1)|(1,2)",
            "1.0|LINK(42,1)|(1,2)",
            "1.0|CREATE(1,x,0,0,99,0,0,,,true):2|(1,2)",
        ] {
            assert!(matches!(
                parse_record(record),
                Err(ChangeSourceError::MalformedRecord(_))
            ));
        }
        assert_eq!(parse_record("1.0|INCR_COUNTER(usedSpace,1)|(1,2)").unwrap(), None);
    }
}
