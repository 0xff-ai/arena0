//! Exercise the blob imports with real Wasm calls and an in-memory view.

use super::*;
use arena0_protocol::execution::{BlobChange, BlobPartial, MAX_DIRECT_QUEUE};
use arena0_protocol::{
    Attachment, BlobHash, Committed, CvSource, Ensemble, PeerId, RangeAttachment,
};
use std::{ops::Range, sync::Arc};
use wasmtime::{Config, Engine, Instance, Linker, Module, Store};

/// An in-memory blob view: granted blobs with their bytes, and partials with
/// their length, durable bytes `[0, written)`, and committed flag.
#[derive(Default)]
pub(crate) struct View {
    pub(crate) granted: Vec<(BlobHash, Vec<u8>)>,
    pub(crate) partials: Vec<(BlobHash, u64, Vec<u8>, bool)>,
    // Model a granted file that cannot supply its advertised bytes.
    pub(crate) short_read: bool,
}

impl crate::BlobView for View {
    fn granted(&self, hash: BlobHash) -> Result<Option<u64>, String> {
        Ok(self
            .granted
            .iter()
            .find(|(h, _)| *h == hash)
            .map(|(_, bytes)| bytes.len() as u64))
    }
    fn read(&self, hash: BlobHash, range: Range<u64>) -> Result<Option<Vec<u8>>, String> {
        if self.short_read {
            return Ok(None);
        }
        Ok(self
            .granted
            .iter()
            .find(|(h, _)| *h == hash)
            .and_then(|(_, bytes)| {
                bytes
                    .get(range.start as usize..range.end as usize)
                    .map(<[u8]>::to_vec)
            }))
    }
    fn partial(&self, hash: BlobHash) -> Result<Option<BlobPartial>, String> {
        Ok(self.partials.iter().find(|(h, _, _, _)| *h == hash).map(
            |(_, length, bytes, committed)| BlobPartial {
                length: *length,
                written: bytes.len() as u64,
                committed: *committed,
            },
        ))
    }
    fn hash_partial(&self, hash: BlobHash, tail: &[u8]) -> Result<BlobHash, String> {
        let (_, _, bytes, _) = self
            .partials
            .iter()
            .find(|(h, _, _, _)| *h == hash)
            .expect("caller checked the partial");
        let content = [bytes.as_slice(), tail].concat();
        Ok(BlobHash(arena0_crypto::hash(
            arena0_crypto::HashAlgorithm::Blake3,
            &content,
        )))
    }
}

struct Fixture {
    store: Store<HostState>,
    instance: Instance,
}

impl Fixture {
    fn new(view: View, attachment: Option<Vec<u8>>) -> Self {
        let engine = Engine::new(Config::new().consume_fuel(true)).unwrap();
        let module = Module::new(
            &engine,
            r#"
            (module
              (import "arena0" "blob_append" (func $append (param i32 i64 i32) (result i32)))
              (import "arena0" "blob_commit" (func $commit (param i32) (result i32)))
              (import "arena0" "subtree_cv" (func $cv (param i32 i32 i64 i32) (result i32)))
              (import "arena0" "merge_cv" (func $merge (param i32 i32 i32 i32)))
              (import "arena0" "send_direct" (func $send (param i32 i32 i32 i32 i32) (result i32)))
              (memory (export "memory") 2)
              (func (export "append") (param i64 i32) (result i32)
                i32.const 0 local.get 0 local.get 1 call $append)
              (func (export "commit") (result i32) i32.const 0 call $commit)
              (func (export "cv") (param i32 i64) (result i32)
                i32.const 256 local.get 0 local.get 1 i32.const 320 call $cv)
              (func (export "merge") (param i32)
                i32.const 256 i32.const 288 local.get 0 i32.const 320 call $merge)
              (func (export "send") (param i32 i32) (result i32)
                i32.const 64 i32.const 1024 local.get 0 i32.const 65536 local.get 1 call $send))
        "#,
        )
        .unwrap();
        let mut linker = Linker::new(&engine);
        register_always_available(&mut linker).unwrap();
        register_capability_imports(&mut linker, &[Capability::Blobs, Capability::Messaging])
            .unwrap();
        let mut state = HostState::new(
            arena0_program::ExecutionProfile::current(),
            crate::engine::CallKind::Dispatch,
            DispatchKind::Local,
            Vec::new(),
        );
        state.blobs = Some(Arc::new(view));
        state.attachment = attachment;
        state.peer_id = Some(PeerId([1; 32]));
        state.session = Some(
            Ensemble::<Committed>::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).unwrap(),
        );
        let mut store = Store::new(&engine, state);
        store.set_fuel(u64::MAX).unwrap();
        let instance = linker.instantiate(&mut store, &module).unwrap();
        instance
            .get_memory(&mut store, "memory")
            .unwrap()
            .write(&mut store, 64, &[2; 32])
            .unwrap();
        Self { store, instance }
    }

    fn append(&mut self, length: u64, token: u32) -> Result<u32, wasmtime::Error> {
        self.instance
            .get_typed_func::<(u64, u32), u32>(&mut self.store, "append")
            .unwrap()
            .call(&mut self.store, (length, token))
    }
    fn commit(&mut self) -> Result<u32, wasmtime::Error> {
        self.instance
            .get_typed_func::<(), u32>(&mut self.store, "commit")
            .unwrap()
            .call(&mut self.store, ())
    }
    fn cv(&mut self, source: CvSource, offset: u64) -> Result<(u32, [u8; 32]), wasmtime::Error> {
        let bytes = borsh::to_vec(&source).unwrap();
        let memory = self.instance.get_memory(&mut self.store, "memory").unwrap();
        memory.write(&mut self.store, 256, &bytes).unwrap();
        let status = self
            .instance
            .get_typed_func::<(u32, u64), u32>(&mut self.store, "cv")
            .unwrap()
            .call(&mut self.store, (bytes.len() as u32, offset))?;
        let mut cv = [0; 32];
        memory.read(&self.store, 320, &mut cv).unwrap();
        Ok((status, cv))
    }
    fn merge(&mut self, left: [u8; 32], right: [u8; 32], root: bool) -> [u8; 32] {
        let memory = self.instance.get_memory(&mut self.store, "memory").unwrap();
        memory.write(&mut self.store, 256, &left).unwrap();
        memory.write(&mut self.store, 288, &right).unwrap();
        self.instance
            .get_typed_func::<u32, ()>(&mut self.store, "merge")
            .unwrap()
            .call(&mut self.store, u32::from(root))
            .unwrap();
        let mut cv = [0; 32];
        memory.read(&self.store, 320, &mut cv).unwrap();
        cv
    }
    fn send(&mut self, len: u32, range: Option<RangeAttachment>) -> Result<u32, wasmtime::Error> {
        let encoded = range
            .map(|r| borsh::to_vec(&r).unwrap())
            .unwrap_or_default();
        self.instance
            .get_memory(&mut self.store, "memory")
            .unwrap()
            .write(&mut self.store, 65536, &encoded)
            .unwrap();
        self.instance
            .get_typed_func::<(u32, u32), u32>(&mut self.store, "send")
            .unwrap()
            .call(&mut self.store, (len, encoded.len() as u32))
    }
}

#[test]
fn append_stages_after_durable_bytes_in_call_order() {
    let hash = BlobHash([0; 32]);
    let mut f = Fixture::new(
        View {
            partials: vec![(hash, 10, vec![1; 4], false)],
            ..View::default()
        },
        Some(vec![2; 3]),
    );
    assert_eq!(f.append(10, 0).unwrap(), 0);
    assert_eq!(f.append(10, 0).unwrap(), 0);
    assert_eq!(f.append(10, 0).unwrap(), 3);
    assert_eq!(f.append(arena0_protocol::MAX_BLOB_BYTES + 1, 0).unwrap(), 2);
    assert_eq!(f.append(10, 1).unwrap(), 4);
    assert_eq!(f.append(11, 0).unwrap(), 3);
    assert_eq!(
        f.store.data().staged_blobs,
        vec![
            BlobChange::Append {
                hash,
                length: 10,
                offset: 4,
                bytes: vec![2; 3]
            },
            BlobChange::Append {
                hash,
                length: 10,
                offset: 7,
                bytes: vec![2; 3]
            },
        ]
    );
    assert_eq!(f.store.data().attachment, Some(vec![2; 3]));
    for attachment in [None, Some(Vec::new())] {
        let mut f = Fixture::new(View::default(), attachment);
        assert_eq!(f.append(10, 0).unwrap(), 4);
        assert!(f.store.data().staged_blobs.is_empty());
    }
    let mut f = Fixture::new(
        View {
            partials: vec![(hash, 10, vec![1; 10], true)],
            ..View::default()
        },
        Some(vec![2; 3]),
    );
    assert_eq!(f.append(10, 0).unwrap(), 3);
    assert!(f.store.data().staged_blobs.is_empty());
}

#[test]
fn commit_hashes_durable_and_staged_bytes() {
    let hash = BlobHash(arena0_crypto::hash(
        arena0_crypto::HashAlgorithm::Blake3,
        b"abcd",
    ));
    for durable in [false, true] {
        let view = if durable {
            View {
                partials: vec![(hash, 4, b"ab".to_vec(), false)],
                ..View::default()
            }
        } else {
            View::default()
        };
        let mut f = Fixture::new(
            view,
            Some(if durable {
                b"cd".to_vec()
            } else {
                b"abcd".to_vec()
            }),
        );
        f.instance
            .get_memory(&mut f.store, "memory")
            .unwrap()
            .write(&mut f.store, 0, &hash.0)
            .unwrap();
        assert_eq!(f.commit().unwrap(), if durable { 5 } else { 1 });
        assert_eq!(f.append(4, 0).unwrap(), 0);
        assert_eq!(f.commit().unwrap(), 0);
        let expected = vec![
            BlobChange::Append {
                hash,
                length: 4,
                offset: if durable { 2 } else { 0 },
                bytes: if durable {
                    b"cd".to_vec()
                } else {
                    b"abcd".to_vec()
                },
            },
            BlobChange::Commit { hash },
        ];
        assert_eq!(f.store.data().staged_blobs, expected);
        assert_eq!(f.commit().unwrap(), 3);
        assert_eq!(f.append(4, 0).unwrap(), 3);
        assert_eq!(f.store.data().staged_blobs, expected);
    }
    let mut f = Fixture::new(View::default(), Some(vec![1]));
    assert_eq!(f.append(2, 0).unwrap(), 0);
    assert_eq!(f.commit().unwrap(), 5);
    assert_eq!(f.append(2, 0).unwrap(), 0);
    let before = f.store.data().staged_blobs.clone();
    assert_eq!(f.commit().unwrap(), 6);
    assert_eq!(f.store.data().staged_blobs, before);
}

#[test]
fn subtree_cv_hashes_granted_blobs_and_the_attachment() {
    let hash = BlobHash([7; 32]);
    let bytes = vec![9; 4096];
    let mut f = Fixture::new(
        View {
            granted: vec![(hash, bytes.clone())],
            ..View::default()
        },
        Some(bytes.clone()),
    );
    let source = CvSource::Blob {
        hash,
        start: 1024,
        end: 3072,
    };
    assert_eq!(
        f.cv(source, 32768).unwrap(),
        (
            0,
            arena0_crypto::blake3_tree::subtree_cv(&bytes[1024..3072], 32768)
        )
    );
    assert_eq!(
        f.cv(CvSource::Attachment(Attachment(0)), 0).unwrap(),
        (0, arena0_crypto::blake3_tree::subtree_cv(&bytes, 0))
    );
    assert_eq!(
        f.cv(
            CvSource::Blob {
                hash: BlobHash([8; 32]),
                start: 0,
                end: 1
            },
            0
        )
        .unwrap()
        .0,
        1
    );
    for (start, end, offset) in [
        (0, 1, 1),
        (0, arena0_protocol::MAX_DIRECT_RANGE_BYTES + 1, 0),
        (0, 4097, 0),
        (0, 2048, 1024),
        (1, 1, 0),
    ] {
        assert_eq!(
            f.cv(CvSource::Blob { hash, start, end }, offset).unwrap().0,
            3
        );
    }
    assert_eq!(f.cv(CvSource::Attachment(Attachment(1)), 0).unwrap().0, 4);
    let mut f = Fixture::new(
        View {
            granted: vec![(hash, bytes)],
            short_read: true,
            ..View::default()
        },
        None,
    );
    assert_eq!(f.cv(source, 0).unwrap().0, 1);
}

#[test]
fn merge_cv_is_linked_without_capabilities() {
    let engine = Engine::new(Config::new().consume_fuel(true)).unwrap();
    let module = Module::new(
        &engine,
        r#"(module
        (import "arena0" "merge_cv" (func $merge (param i32 i32 i32 i32)))
        (memory (export "memory") 1)
        (func (export "merge") (param i32)
          i32.const 256 i32.const 288 local.get 0 i32.const 320 call $merge))"#,
    )
    .unwrap();
    let mut linker = Linker::new(&engine);
    register_always_available(&mut linker).unwrap();
    let state = HostState::new(
        arena0_program::ExecutionProfile::current(),
        crate::engine::CallKind::Metadata,
        DispatchKind::Local,
        Vec::new(),
    );
    let mut store = Store::new(&engine, state);
    store.set_fuel(u64::MAX).unwrap();
    let instance = linker.instantiate(&mut store, &module).unwrap();
    let mut f = Fixture { store, instance };
    let left = arena0_crypto::blake3_tree::subtree_cv(&[1; 1024], 0);
    let right = arena0_crypto::blake3_tree::subtree_cv(&[2; 1024], 1024);
    for root in [false, true] {
        assert_eq!(
            f.merge(left, right, root),
            arena0_crypto::blake3_tree::merge_cv(&left, &right, root)
        );
    }
}

#[test]
fn blob_imports_trap_outside_local_dispatch() {
    for case in 0..4 {
        let mut f = Fixture::new(View::default(), Some(vec![1]));
        match case {
            0 => f.store.data_mut().call_kind = crate::engine::CallKind::Metadata,
            1 => f.store.data_mut().call_kind = crate::engine::CallKind::Initialize,
            2 => f.store.data_mut().dispatch = DispatchKind::Agreed,
            3 => f.store.data_mut().blobs = None,
            _ => unreachable!(),
        }
        assert!(f.append(1, 0).is_err());
        assert!(f.commit().is_err());
        assert!(f.cv(CvSource::Attachment(Attachment(0)), 0).is_err());
    }
}

#[test]
fn send_direct_checks_recipient_bounds_range_and_queue() {
    let hash = BlobHash([5; 32]);
    let length = arena0_protocol::MAX_DIRECT_RANGE_BYTES + 1;
    let mut f = Fixture::new(
        View {
            granted: vec![(hash, vec![0; length as usize])],
            ..View::default()
        },
        None,
    );
    for peer in [[1; 32], [3; 32]] {
        f.instance
            .get_memory(&mut f.store, "memory")
            .unwrap()
            .write(&mut f.store, 64, &peer)
            .unwrap();
        assert!(f.send(1, None).is_err());
    }
    f.instance
        .get_memory(&mut f.store, "memory")
        .unwrap()
        .write(&mut f.store, 64, &[2; 32])
        .unwrap();
    assert!(
        f.send(arena0_protocol::MAX_DIRECT_CONTROL_BYTES as u32 + 1, None)
            .is_err()
    );
    let range = RangeAttachment {
        hash,
        start: 0,
        end: arena0_protocol::MAX_DIRECT_RANGE_BYTES,
    };
    assert!(
        f.send(
            1,
            Some(RangeAttachment {
                hash: BlobHash([6; 32]),
                ..range
            })
        )
        .is_err()
    );
    for (start, end) in [(1, 1), (0, length), (length - 1, length + 1)] {
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
    assert_eq!(
        f.send(
            arena0_protocol::MAX_DIRECT_CONTROL_BYTES as u32,
            Some(range)
        )
        .unwrap(),
        0
    );
    assert!(
        matches!(&f.store.data().effect_queue[0], Effect::SendDirect { range: Some(r), .. } if *r == range)
    );
    f.store.data_mut().direct_queued = vec![(PeerId([2; 32]), MAX_DIRECT_QUEUE - 2)];
    assert_eq!(f.send(1, None).unwrap(), 0);
    assert_eq!(f.send(1, None).unwrap(), 1);
    assert_eq!(f.store.data().effect_queue.len(), 2);
    f.store.data_mut().dispatch = DispatchKind::Agreed;
    assert!(f.send(1, None).is_err());
}
