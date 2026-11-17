//! Versioned chunk wire goldens and an opt-in encoding kernel benchmark.
use super::java;
use anyhow::Result;
use sculith_world::{
    BlockEdit, BlockKind, BlockPos, ChunkPos, ChunkSnapshot, MAX_EDITS_PER_BATCH, MAX_Y, MIN_Y,
    Region,
};
use serde_json::{Value, json};
use std::{hint::black_box, path::PathBuf, time::Instant};

const GOLDENS: &str = "../../tests/fixtures/chunk-encoding-v777-v2225.json";
#[derive(Clone, Copy)]
enum Encoding {
    Java,
    Light,
    Bedrock,
}
impl Encoding {
    const ALL: [Self; 3] = [Self::Java, Self::Light, Self::Bedrock];
    fn name(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Light => "java-light",
            Self::Bedrock => "bedrock",
        }
    }
    fn encode(self, snapshot: &ChunkSnapshot) -> Result<Vec<u8>> {
        match self {
            Self::Java => java::encode_chunk_for_tests(snapshot),
            Self::Light => java::encode_light_for_tests(snapshot),
            Self::Bedrock => {
                use ::bedrock::protocol::{PacketHeader, Packets, V2225};
                let packet = super::bedrock::encode_chunk_for_tests(snapshot)?;
                let mut bytes = Vec::new();
                V2225::LevelChunkPacket(Box::new(packet)).serialize(
                    &PacketHeader {
                        packet_id: 58,
                        ..Default::default()
                    },
                    &mut bytes,
                )?;
                Ok(bytes)
            }
        }
    }
}
fn hash(bytes: &[u8]) -> String {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn apply(owner: &mut Region, edits: &[BlockEdit]) {
    for batch in edits.chunks(MAX_EDITS_PER_BATCH) {
        owner.commit(owner.revision(), batch).unwrap();
    }
}
fn point(position: ChunkPos, x: i32, y: i32, z: i32, block: BlockKind) -> BlockEdit {
    BlockEdit {
        position: BlockPos {
            x: position.x * 16 + x,
            y,
            z: position.z * 16 + z,
        },
        block,
    }
}
fn clear_ground(owner: &mut Region, position: ChunkPos) {
    let mut edits = Vec::new();
    for y in 0..4 {
        for z in 0..16 {
            for x in 0..16 {
                edits.push(point(position, x, y, z, BlockKind::Air));
            }
        }
    }
    apply(owner, &edits);
}
fn fixtures() -> Vec<(&'static str, ChunkSnapshot)> {
    let mut owner = Region::flat_origin();
    let flat = owner.snapshot(ChunkPos { x: -2, z: 1 }).unwrap();
    let empty_position = ChunkPos { x: 0, z: 0 };
    clear_ground(&mut owner, empty_position);
    let empty = owner.snapshot(empty_position).unwrap();
    let sparse_position = ChunkPos { x: 1, z: -1 };
    clear_ground(&mut owner, sparse_position);
    let solid_kinds = [
        BlockKind::Bedrock,
        BlockKind::Stone,
        BlockKind::Dirt,
        BlockKind::Grass,
    ];
    let mut edits = Vec::new();
    for z in 0..16 {
        for x in 0..16 {
            if (x + z) % 7 == 0 {
                continue;
            }
            let y = if (x, z) == (15, 15) {
                MAX_Y
            } else if (x, z) == (0, 1) {
                MIN_Y
            } else {
                MIN_Y + (x * 37 + z * 19) % 384
            };
            edits.push(point(
                sparse_position,
                x,
                y,
                z,
                solid_kinds[((x + 2 * z) % 4) as usize],
            ));
        }
    }
    apply(&mut owner, &edits);
    let sparse = owner.snapshot(sparse_position).unwrap();
    let solid_position = ChunkPos { x: -1, z: 0 };
    let dense_position = ChunkPos { x: -1, z: -1 };
    let kinds = [
        BlockKind::Air,
        BlockKind::Bedrock,
        BlockKind::Stone,
        BlockKind::Dirt,
        BlockKind::Grass,
    ];
    let mut solid = Vec::new();
    let mut dense = Vec::new();
    for y in MIN_Y..=MAX_Y {
        for z in 0..16 {
            for x in 0..16 {
                solid.push(point(solid_position, x, y, z, BlockKind::Stone));
                dense.push(point(
                    dense_position,
                    x,
                    y,
                    z,
                    kinds[(x * 7 + y * 11 + z * 3).rem_euclid(5) as usize],
                ));
            }
        }
    }
    apply(&mut owner, &solid);
    apply(&mut owner, &dense);
    vec![
        ("flat", flat),
        ("empty", empty),
        ("sparse", sparse),
        ("solid", owner.snapshot(solid_position).unwrap()),
        ("dense", owner.snapshot(dense_position).unwrap()),
    ]
}
fn read_goldens() -> Value {
    serde_json::from_slice(
        &std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDENS)).unwrap(),
    )
    .unwrap()
}
fn verify(goldens: &Value, name: &str, encoding: Encoding, bytes: &[u8]) {
    let reference = &goldens["cases"][name][encoding.name()];
    assert_eq!(
        reference["bytes"].as_u64().unwrap(),
        bytes.len() as u64,
        "{name} {} length",
        encoding.name()
    );
    assert_eq!(
        reference["sha256"].as_str().unwrap(),
        hash(bytes),
        "{name} {} wire bytes",
        encoding.name()
    );
}
#[test]
fn chunk_encoders_preserve_versioned_wire_goldens() {
    let goldens = read_goldens();
    let refs = super::references();
    assert_eq!(goldens["java_protocol"], refs.java.protocol);
    assert_eq!(goldens["bedrock_protocol"], refs.bedrock.protocol);
    assert_eq!(goldens["cases"].as_object().unwrap().len(), 5);
    for (name, snapshot) in fixtures() {
        for encoding in Encoding::ALL {
            verify(
                &goldens,
                name,
                encoding,
                &encoding.encode(&snapshot).unwrap(),
            );
        }
    }
}
#[test]
#[ignore = "explicit reference fixture generation; review before updating"]
fn record_reference_encodings() {
    let refs = super::references();
    let mut cases = serde_json::Map::new();
    for (name, snapshot) in fixtures() {
        let mut encoded = serde_json::Map::new();
        for encoding in Encoding::ALL {
            let bytes = encoding.encode(&snapshot).unwrap();
            encoded.insert(
                encoding.name().into(),
                json!({"bytes":bytes.len(),"sha256":hash(&bytes)}),
            );
        }
        cases.insert(name.into(), Value::Object(encoded));
    }
    println!(
        "SCULITH_CHUNK_GOLDENS {}",
        json!({"java_protocol":refs.java.protocol,"bedrock_protocol":refs.bedrock.protocol,"cases":cases})
    );
}
#[cfg(target_os = "linux")]
fn cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).ok()?;
    let fields = stat
        .rsplit_once(") ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(fields.get(12)?.parse().ok()?)
}
#[cfg(not(target_os = "linux"))]
fn cpu_ticks() -> Option<u64> {
    None
}
#[test]
#[ignore = "release encoding kernel measurement, not game TPS"]
fn benchmark_encoders() {
    let iterations = std::env::var("SCULITH_CHUNK_BENCH_ITERATIONS")
        .unwrap_or("1000".into())
        .parse::<usize>()
        .unwrap();
    assert!((256..=10000).contains(&iterations));
    let goldens = read_goldens();
    for (name, snapshot) in fixtures() {
        for encoding in Encoding::ALL {
            let case = format!("{name}-{}", encoding.name());
            let expected = encoding.encode(&snapshot).unwrap();
            verify(&goldens, name, encoding, &expected);
            for _ in 0..64 {
                assert_eq!(encoding.encode(black_box(&snapshot)).unwrap(), expected);
            }
            let mut latencies = Vec::with_capacity(iterations);
            println!(
                "{}",
                json!({"kind":"phase_start","case":case,"host_pid":std::process::id(),"worker_pids":[]})
            );
            let cpu = cpu_ticks();
            let start = Instant::now();
            for _ in 0..iterations {
                let at = Instant::now();
                let bytes = encoding.encode(black_box(&snapshot)).unwrap();
                latencies.push(at.elapsed().as_nanos() as u64);
                assert_eq!(
                    black_box(bytes.as_slice()),
                    expected.as_slice(),
                    "encoder changed its bytes within a measured phase"
                );
            }
            let wall = start.elapsed().as_nanos();
            let cpu = cpu_ticks()
                .zip(cpu)
                .map(|(after, before)| after.saturating_sub(before));
            latencies.sort_unstable();
            let q = |percent: usize| latencies[(iterations * percent).div_ceil(100) - 1];
            println!(
                "{}",
                json!({"kind":"phase_end","case":case,"iterations":iterations,"workers":0,"subscribers":0,"mode":"encoding","delivered_requests":iterations,
            "encoded_bytes":expected.len(),"encoded_sha256":hash(&expected),"wall_ns":wall,"p50_ns":q(50),"p95_ns":q(95),"p99_ns":q(99),"host_cpu_ticks":cpu,"worker_cpu_ticks":0})
            );
        }
    }
}
