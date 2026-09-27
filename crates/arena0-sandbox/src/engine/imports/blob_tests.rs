//! Exercise the guest ABI with real Wasm calls and a committed-store stub.

use super::*;
use arena0_protocol::execution::{BlobChange, BlobResource, MAX_DIRECT_QUEUE};
use arena0_protocol::{BlobHandle, BlobHash, Committed, Ensemble, PeerId, RangeAttachment};
use std::{ops::Range, sync::Arc};
use wasmtime::{Engine, Instance, Linker, Module, Store};

#[derive(Default)]
pub(crate) struct View {
    resources: Vec<(BlobHandle, BlobResource, Vec<Range<u64>>)>,
    content: Vec<(BlobHash, u64)>,
}

impl crate::BlobView for View {
    fn contains(&self, hash: BlobHash, length: u64) -> Result<bool, String> {
        Ok(self.content.contains(&(hash, length)))
    }
    fn resource(&self, handle: BlobHandle) -> Result<Option<BlobResource>, String> {
        Ok(self
            .resources
            .iter()
            .find(|(h, _, _)| *h == handle)
            .map(|(_, r, _)| *r))
    }
    fn written(&self, handle: BlobHandle) -> Result<Vec<Range<u64>>, String> {
        Ok(self
            .resources
            .iter()
            .find(|(h, _, _)| *h == handle)
            .map_or_else(Vec::new, |(_, _, ranges)| ranges.clone()))
    }
}

struct Fixture {
    store: Store<HostState>,
    instance: Instance,
}

impl Fixture {
    fn new(view: View, slice: Option<Vec<u8>>) -> Self {
        let engine = Engine::default();
        let module = Module::new(&engine, r#"
            (module
              (import "arena0" "blob_create" (func $create (param i32 i64 i32) (result i32)))
              (import "arena0" "blob_resolve" (func $resolve (param i32 i64 i32) (result i32)))
              (import "arena0" "blob_accept_range" (func $accept (param i32 i32 i64 i64) (result i32)))
              (import "arena0" "blob_commit" (func $commit (param i32) (result i32)))
              (import "arena0" "send_direct" (func $send (param i32 i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 2)
              (func (export "create") (param i64) (result i32)
                i32.const 0 local.get 0 i32.const 32 call $create)
              (func (export "resolve") (param i64) (result i32)
                i32.const 0 local.get 0 i32.const 32 call $resolve)
              (func (export "accept") (param i32 i64 i64) (result i32)
                i32.const 32 local.get 0 local.get 1 local.get 2 call $accept)
              (func (export "commit") (result i32) i32.const 32 call $commit)
              (func (export "send") (param i32 i32) (result i32)
                i32.const 64 i32.const 128 local.get 0 i32.const 65536 local.get 1 call $send))
        "#).unwrap();
        let mut linker = Linker::new(&engine);
        register_metadata_imports(&mut linker).unwrap();
        let mut state = HostState::new(
            arena0_program::ExecutionProfile::current(),
            crate::engine::CallKind::Dispatch,
            DispatchKind::Local,
            Vec::new(),
        );
        state.blobs = Some(Arc::new(view));
        state.slice = slice;
        state.event_position = 7;
        state.peer_id = Some(PeerId([1; 32]));
        state.session = Some(
            Ensemble::<Committed>::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap(),
        );
        let mut store = Store::new(&engine, state);
        let instance = linker.instantiate(&mut store, &module).unwrap();
        let mut fixture = Self { store, instance };
        fixture.write(64, &[2; 32]);
        fixture
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.instance
            .get_memory(&mut self.store, "memory")
            .unwrap()
            .write(&mut self.store, offset, bytes)
            .unwrap();
    }
    fn hash(&mut self, content: &[u8]) -> BlobHash {
        let hash = BlobHash(arena0_crypto::hash(
            arena0_crypto::HashAlgorithm::Blake3,
            content,
        ));
        self.write(0, &hash.0);
        hash
    }
    fn handle(&mut self) -> BlobHandle {
        let mut bytes = [0; 12];
        self.instance
            .get_memory(&mut self.store, "memory")
            .unwrap()
            .read(&self.store, 32, &mut bytes)
            .unwrap();
        borsh::from_slice(&bytes).unwrap()
    }
    fn mint(&mut self, name: &str, length: u64) -> Result<u32, wasmtime::Error> {
        self.instance
            .get_typed_func::<u64, u32>(&mut self.store, name)
            .unwrap()
            .call(&mut self.store, length)
    }
    fn accept(&mut self, token: u32, start: u64, end: u64) -> Result<u32, wasmtime::Error> {
        self.instance
            .get_typed_func::<(u32, u64, u64), u32>(&mut self.store, "accept")
            .unwrap()
            .call(&mut self.store, (token, start, end))
    }
    fn commit(&mut self) -> Result<u32, wasmtime::Error> {
        self.instance
            .get_typed_func::<(), u32>(&mut self.store, "commit")
            .unwrap()
            .call(&mut self.store, ())
    }
    fn send(&mut self, len: u32, range: Option<RangeAttachment>) -> Result<u32, wasmtime::Error> {
        let encoded = range
            .map(|r| borsh::to_vec(&r).unwrap())
            .unwrap_or_default();
        self.write(65536, &encoded);
        self.instance
            .get_typed_func::<(u32, u32), u32>(&mut self.store, "send")
            .unwrap()
            .call(&mut self.store, (len, encoded.len() as u32))
    }
}

#[test]
fn blob_create_accept_commit_stages_changes_in_call_order() {
    let content = vec![0x51; 2048];
    let mut f = Fixture::new(
        View::default(),
        Some(arena0_crypto::bao::encode_slice(&content, 0, 2048)),
    );
    let hash = f.hash(&content);
    assert_eq!(f.mint("create", 2048).unwrap(), 0);
    let handle = f.handle();
    assert_eq!(
        handle,
        BlobHandle {
            event_position: 7,
            call_index: 0
        }
    );
    assert_eq!(f.accept(0, 0, 2048).unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 3);
    assert_eq!(
        f.mint("resolve", 2048).unwrap(),
        0,
        "staged commit is contained"
    );
    let resolved = f.handle();
    assert_eq!(resolved.call_index, 1);
    assert_eq!(
        f.store.data().staged_blobs,
        vec![
            BlobChange::Create {
                handle,
                hash,
                length: 2048
            },
            BlobChange::Write {
                handle,
                offset: 0,
                bytes: content
            },
            BlobChange::Commit { handle },
            BlobChange::Resolve {
                handle: resolved,
                hash,
                length: 2048
            },
        ]
    );
    assert_eq!(
        f.send(
            1,
            Some(RangeAttachment {
                source: resolved,
                start: 0,
                end: 2048
            })
        )
        .unwrap(),
        0
    );
}

#[test]
fn accept_range_rejects_bad_slices_overlap_and_second_use() {
    let content = vec![9; 4096];
    let proof = arena0_crypto::bao::encode_slice(&content, 0, 2048);
    for (start, end, expected) in [(0, 0, 3), (0, 4097, 3), (2048, 4096, 4)] {
        let mut f = Fixture::new(View::default(), Some(proof.clone()));
        f.hash(&content);
        assert_eq!(f.mint("create", 4096).unwrap(), 0);
        assert_eq!(f.accept(0, start, end).unwrap(), expected);
        assert_eq!(
            f.accept(0, 0, 2048).unwrap(),
            4,
            "failed range/proof consumed attachment"
        );
        assert_eq!(f.store.data().staged_blobs.len(), 1);
    }
    let mut f = Fixture::new(View::default(), Some(proof.clone()));
    f.hash(&content);
    assert_eq!(f.mint("create", 4096).unwrap(), 0);
    assert_eq!(f.accept(1, 0, 2048).unwrap(), 4);
    assert_eq!(
        f.accept(0, 0, 2048).unwrap(),
        0,
        "wrong token did not consume token zero"
    );
    assert_eq!(f.accept(0, 0, 2048).unwrap(), 4);

    let handle = BlobHandle {
        event_position: 6,
        call_index: 0,
    };
    let resource = BlobResource {
        hash: BlobHash(arena0_crypto::hash(
            arena0_crypto::HashAlgorithm::Blake3,
            &content,
        )),
        length: 4096,
        output: true,
        committed: false,
    };
    let mut f = Fixture::new(
        View {
            resources: vec![(handle, resource, std::iter::once(1024..2048).collect())],
            ..View::default()
        },
        Some(proof),
    );
    f.write(32, &borsh::to_vec(&handle).unwrap());
    assert_eq!(f.accept(0, 0, 2048).unwrap(), 3, "stored overlap");
    assert!(f.store.data().staged_blobs.is_empty());
    assert!(f.store.data().slice.is_none());

    let mut bad = arena0_crypto::bao::encode_slice(&content, 0, 2048);
    *bad.last_mut().unwrap() ^= 1;
    let mut f = Fixture::new(View::default(), Some(bad));
    f.hash(&content);
    f.mint("create", 4096).unwrap();
    assert_eq!(f.accept(0, 0, 2048).unwrap(), 4);

    let mut f = Fixture::new(
        View::default(),
        Some(arena0_crypto::bao::encode_slice(&content, 0, 2048)),
    );
    f.hash(&content);
    f.mint("create", 4096).unwrap();
    f.store.data_mut().profile.limits.max_host_bytes = 100;
    assert!(
        f.accept(0, 0, 2048).is_err(),
        "decoded bytes consume the host budget before decoding"
    );
    assert_eq!(f.store.data().staged_blobs.len(), 1);
}

#[test]
fn commit_requires_full_coverage_and_empty_object_hash() {
    let mut f = Fixture::new(View::default(), None);
    f.hash(b"");
    assert_eq!(f.mint("create", 0).unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 0);
    f.write(0, &[0; 32]);
    assert_eq!(f.mint("create", 0).unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 4);
    assert_eq!(f.mint("create", 10).unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 5);
    assert_eq!(
        f.mint("create", arena0_protocol::MAX_BLOB_BYTES + 1)
            .unwrap(),
        2
    );

    let handle = BlobHandle {
        event_position: 6,
        call_index: 0,
    };
    let resource = BlobResource {
        hash: BlobHash([9; 32]),
        length: 10,
        output: true,
        committed: false,
    };
    for (ranges, expected) in [(vec![0..4, 5..10], 5), (vec![0..4, 4..10], 0)] {
        let mut f = Fixture::new(
            View {
                resources: vec![(handle, resource, ranges)],
                ..View::default()
            },
            None,
        );
        f.write(32, &borsh::to_vec(&handle).unwrap());
        assert_eq!(f.commit().unwrap(), expected);
    }
}

#[test]
fn resolve_finds_committed_content_and_mints_rerun_stable_handles() {
    let hash = BlobHash([3; 32]);
    let mut handles = Vec::new();
    for _ in 0..2 {
        let mut f = Fixture::new(
            View {
                content: vec![(hash, 10)],
                ..View::default()
            },
            None,
        );
        f.write(0, &hash.0);
        assert_eq!(f.mint("resolve", 11).unwrap(), 1);
        assert_eq!(
            f.mint("resolve", arena0_protocol::MAX_BLOB_BYTES + 1)
                .unwrap(),
            2
        );
        assert_eq!(f.mint("resolve", 10).unwrap(), 0);
        handles.push(f.handle());
        assert_eq!(f.commit().unwrap(), 3, "resolved content is not an output");
    }
    assert_eq!(
        handles,
        vec![
            BlobHandle {
                event_position: 7,
                call_index: 0
            };
            2
        ]
    );
}

#[test]
fn blob_imports_trap_outside_local_dispatch() {
    for call_kind in [
        crate::engine::CallKind::Metadata,
        crate::engine::CallKind::Initialize,
    ] {
        let mut f = Fixture::new(View::default(), None);
        f.store.data_mut().call_kind = call_kind;
        assert!(f.mint("create", 0).is_err());
        assert!(f.mint("resolve", 0).is_err());
        assert!(f.accept(0, 0, 1).is_err());
        assert!(f.commit().is_err());
    }
    let mut f = Fixture::new(View::default(), None);
    f.store.data_mut().dispatch = DispatchKind::Agreed;
    assert!(f.mint("create", 0).is_err());
    assert!(f.mint("resolve", 0).is_err());
    assert!(f.accept(0, 0, 1).is_err());
    assert!(f.commit().is_err());
    let mut f = Fixture::new(View::default(), None);
    f.store.data_mut().blobs = None;
    assert!(f.mint("create", 0).is_err());
    assert!(f.mint("resolve", 0).is_err());
    assert!(f.accept(0, 0, 1).is_err());
    assert!(f.commit().is_err());
}

#[test]
fn send_direct_checks_recipient_bounds_range_and_queue() {
    let mut f = Fixture::new(View::default(), None);
    for peer in [[1; 32], [3; 32]] {
        f.write(64, &peer);
        assert!(f.send(1, None).is_err());
    }
    f.write(64, &[2; 32]);
    assert!(
        f.send(arena0_protocol::MAX_DIRECT_CONTROL_BYTES as u32 + 1, None)
            .is_err()
    );
    assert_eq!(
        f.send(arena0_protocol::MAX_DIRECT_CONTROL_BYTES as u32, None)
            .unwrap(),
        0
    );
    f.store.data_mut().direct_queued = vec![(PeerId([2; 32]), MAX_DIRECT_QUEUE - 2)];
    assert_eq!(f.send(1, None).unwrap(), 0);
    assert_eq!(f.send(1, None).unwrap(), 1);
    assert_eq!(f.store.data().effect_queue.len(), 2);
    f.store.data_mut().dispatch = DispatchKind::Agreed;
    assert!(f.send(1, None).is_err());

    let handle = BlobHandle {
        event_position: 6,
        call_index: 0,
    };
    for committed in [false, true] {
        let resource = BlobResource {
            hash: BlobHash([5; 32]),
            length: arena0_protocol::MAX_BLOB_BYTES,
            output: true,
            committed,
        };
        let mut f = Fixture::new(
            View {
                resources: vec![(handle, resource, Vec::new())],
                ..View::default()
            },
            None,
        );
        let range = RangeAttachment {
            source: handle,
            start: 0,
            end: arena0_protocol::MAX_DIRECT_RANGE_BYTES,
        };
        if committed {
            assert_eq!(f.send(1, Some(range)).unwrap(), 0);
        } else {
            assert!(f.send(1, Some(range)).is_err());
        }
        for (start, end) in [
            (1, 1),
            (0, arena0_protocol::MAX_DIRECT_RANGE_BYTES + 1),
            (
                arena0_protocol::MAX_BLOB_BYTES - 1,
                arena0_protocol::MAX_BLOB_BYTES + 1,
            ),
        ] {
            assert!(
                f.send(
                    1,
                    Some(RangeAttachment {
                        start,
                        end,
                        ..range
                    })
                )
                .is_err()
            );
        }
    }
}
