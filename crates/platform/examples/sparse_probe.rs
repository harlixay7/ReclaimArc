//! Diagnostic: measure allocation via three methods at each lifecycle step.
//! Usage: cargo run -p spacextract-platform --example sparse_probe <path>
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use spacextract_platform::fs::{allocated_size, allocated_size_from_handle};
use spacextract_platform::sparse::{
    align_inward, open_for_reclaim, query_allocated_ranges, set_sparse, zero_range, ByteRange,
};

fn report(file: &std::fs::File, path: &Path, label: &str, total: u64) {
    let by_path = allocated_size(path).unwrap();
    let by_handle = allocated_size_from_handle(file, path).unwrap();
    let q: Vec<ByteRange> = query_allocated_ranges(file, path, 0, total).unwrap();
    let qsum: u64 = q.iter().map(|r| r.len).sum();
    println!("{label}: by-path={by_path} by-handle={by_handle} query-sum={qsum} ranges={q:?}");
}

fn main() {
    let arg = std::env::args().nth(1).expect("usage: sparse_probe <path>");
    let path = Path::new(&arg);
    let total = 64u64 * 65536;
    let mut file = open_for_reclaim(path).unwrap();
    let mut buf = vec![0u8; 65536];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i * 7 % 251) as u8;
    }
    for _ in 0..64 {
        file.write_all(&buf).unwrap();
    }
    file.sync_all().unwrap();
    report(&file, path, "written+synced", total);

    set_sparse(&file, path).unwrap();
    report(&file, path, "after set_sparse", total);

    let cluster = spacextract_platform::fs::cluster_size(path).unwrap();
    println!("cluster={cluster}");
    let middle = ByteRange { start: total / 4, len: total / 2 };
    let aligned = align_inward(middle, cluster).unwrap();
    zero_range(&file, path, aligned).unwrap();
    file.sync_all().unwrap();
    report(&file, path, "after zero_range+sync", total);
    file.sync_all().unwrap();
    report(&file, path, "after zero_range+sync(again)", total);
}
