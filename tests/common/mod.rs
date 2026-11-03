use std::fs;
use std::path::PathBuf;

use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;

pub struct ArchiveFile(PathBuf);

impl ArchiveFile {
    pub fn new(bytes: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("carina-docs-{}.tar.gz", uuid::Uuid::new_v4()));
        fs::write(&path, bytes).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ArchiveFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn bundle(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut tar = Vec::new();
    for (path, body) in files {
        let mut header = [0_u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        for (start, end, value) in [
            (100, 108, 0o644),
            (108, 116, 0),
            (116, 124, 0),
            (124, 136, body.len() as u64),
            (136, 148, 0),
        ] {
            let encoded = format!("{:0width$o}\0", value, width = end - start - 1);
            header[start..end].copy_from_slice(encoded.as_bytes());
        }
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(body);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.resize(tar.len() + 1024, 0);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&tar).unwrap();
    encoder.finish().unwrap()
}

pub fn output_overflow() -> Vec<u8> {
    let mut markdown = String::new();
    for _ in 0..1_500 {
        markdown.push_str(&"> ".repeat(60));
        markdown.push_str("x\n\noutside\n\n");
    }
    assert!(markdown.len() < 256 * 1024);
    bundle(&[("index.md", markdown.as_bytes())])
}
