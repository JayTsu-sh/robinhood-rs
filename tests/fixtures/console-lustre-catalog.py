"""Seed only the directory chain leading to the migration test fixture.

A full Lustre mount is required for FID access. The daemon skips the initial
whole-filesystem scan for an already indexed filesystem, then Playwright
requests scans of the fixture subdirectory as it creates test files.
"""

import json
import struct
import sys
from pathlib import Path


def sql_bytes(value: bytes) -> str:
    return f"X'{value.hex()}'"


def fid_bytes(fid: str) -> str:
    words = [int(part, 16) for part in fid.strip('[]').split(':')]
    return sql_bytes(struct.pack('>QII', *words))


mount = Path(sys.argv[1])
capabilities = sys.argv[2]
filesystem = 'execution-fixture'
print('START TRANSACTION;')
print(f"INSERT INTO filesystems (id,backend_kind,mount_path,capabilities) VALUES"
      f" ('{filesystem}','lustre','/mnt/qa-lustre',{sql_bytes(capabilities.encode())});")
parent_key = None
parent_id = 'NULL'
for line in sys.stdin:
    relative, fid = line.rstrip('\n').split('\t')
    path = Path(relative)
    depth = len(path.parts)
    name = path.name
    metadata = (mount / path).stat()
    object_id = fid_bytes(fid)
    key = {'filesystem': filesystem, 'object': {'lustre': fid}}
    entry = {
        'key': key, 'parent': parent_key, 'name': name, 'kind': 'Directory',
        'size': metadata.st_size, 'blocks': metadata.st_blocks,
        'uid': metadata.st_uid, 'gid': metadata.st_gid, 'projid': 0,
        'mode': metadata.st_mode, 'nlink': metadata.st_nlink,
        'atime': int(metadata.st_atime), 'mtime': int(metadata.st_mtime), 'ctime': int(metadata.st_ctime),
        'stripe_count': None, 'stripe_size': None, 'stripe_items': [], 'pool_name': None,
        'sm_status': {}, 'last_seen': int(metadata.st_ctime), 'depth': depth,
    }
    entry_data = sql_bytes(json.dumps(entry).encode())
    name_bytes = sql_bytes(name.encode())
    parent_kind = 'NULL' if parent_key is None else '0'
    print(f"INSERT INTO scoped_entries (filesystem_id,object_kind,object_id,entry_data,parent_kind,"
          f"parent_id,fid,name,kind,size,depth) VALUES ('{filesystem}',0,{object_id},{entry_data},"
          f"{parent_kind},{parent_id},{object_id},{name_bytes},1,{metadata.st_size},{depth});")
    if parent_key is not None:
        print(f"INSERT INTO scoped_namespace_edges (filesystem_id,parent_kind,parent_id,name,object_kind,object_id)"
              f" VALUES ('{filesystem}',0,{parent_id},{name_bytes},0,{object_id});")
    parent_id = object_id
    parent_key = key
if parent_key is None:
    raise ValueError('Lustre directory chain is empty')
print('COMMIT;')
