use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rbh_change_source::juicefs_proto::changelog_server::{Changelog, ChangelogServer};
use rbh_change_source::juicefs_proto::{AckRequest, AckResponse, ChangelogRecord, WatchRequest};
use rbh_change_source::{Change, ChangeSource, JuiceFsChangeSource};
use rbh_entry_store::{EntryKind, FileSystemId, ObjectId};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

#[derive(Default)]
struct AgentState {
    watched_volumes: Mutex<Vec<String>>,
    acknowledged: Mutex<Vec<(String, i64)>>,
    watch_calls: AtomicUsize,
    fail_next_ack: AtomicBool,
}

#[derive(Clone)]
struct TestAgent {
    state: Arc<AgentState>,
}

#[tonic::async_trait]
impl Changelog for TestAgent {
    type WatchStream = ReceiverStream<Result<ChangelogRecord, Status>>;

    async fn watch(&self, request: Request<WatchRequest>) -> Result<Response<Self::WatchStream>, Status> {
        let volume = request.into_inner().volume;
        self.state.watched_volumes.lock().await.push(volume.clone());
        let call = self.state.watch_calls.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let versions: &[i64] = if call == 0 { &[101] } else { &[99, 100, 101, 102] };
        for version in versions {
            tx.send(Ok(ChangelogRecord {
                volume: volume.clone(),
                version: *version,
                entry: "1700000000.000000123|CREATE(1,report%2Cfinal,1000,1000,1,420,18,,,true):42|(7,9)".into(),
            }))
            .await
            .unwrap();
        }
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn ack(&self, request: Request<AckRequest>) -> Result<Response<AckResponse>, Status> {
        if self.state.fail_next_ack.swap(false, Ordering::SeqCst) {
            return Err(Status::unavailable("transient test failure"));
        }
        let request = request.into_inner();
        self.state
            .acknowledged
            .lock()
            .await
            .push((request.volume, request.version));
        Ok(Response::new(AckResponse {}))
    }
}

#[tokio::test]
async fn watch_normalizes_one_volume_and_acknowledges_only_after_commit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Arc::new(AgentState::default());
    let server_state = state.clone();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(ChangelogServer::new(TestAgent { state: server_state }))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    let filesystem = FileSystemId::new("jfs-nfs").unwrap();
    let mut source = JuiceFsChangeSource::connect(filesystem.clone(), format!("http://{address}"), "jfs-nfs".into())
        .await
        .unwrap();

    let batch = source.next_batch().await.unwrap().unwrap();
    assert_eq!(batch.filesystem, filesystem);
    assert_eq!(batch.checkpoint.position, 101);
    assert!(matches!(
        batch.changes.as_slice(),
        [Change::Created {
            object: ObjectId::JuiceFs(42),
            parent: ObjectId::JuiceFs(1),
            name,
            kind: EntryKind::File,
            time: 1_700_000_000,
            ..
        }] if name.as_ref() == b"report,final"
    ));
    assert_eq!(state.watched_volumes.lock().await.as_slice(), ["jfs-nfs"]);
    assert!(state.acknowledged.lock().await.is_empty());

    state.fail_next_ack.store(true, Ordering::SeqCst);
    assert!(source.commit(batch.checkpoint.clone()).await.is_err());
    assert!(state.acknowledged.lock().await.is_empty());
    source.commit(batch.checkpoint).await.unwrap();
    assert_eq!(
        state.acknowledged.lock().await.as_slice(),
        [("jfs-nfs".to_owned(), 101)]
    );

    let replay_safe_batch = source.next_batch().await.unwrap().unwrap();
    assert_eq!(replay_safe_batch.checkpoint.position, 102);
    assert_eq!(state.watched_volumes.lock().await.len(), 2);
    assert_eq!(state.acknowledged.lock().await.len(), 1);
}
