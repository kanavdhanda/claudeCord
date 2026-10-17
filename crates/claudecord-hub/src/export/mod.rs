//! Turning the conversation history into other forms people can read outside the hub. Today there is one: a folder of
//! Markdown notes for Obsidian (`obsidian`), where messages, people, tasks and threads are linked so Obsidian's graph
//! view shows who talked to whom about what.

pub mod obsidian;

/// The files as one `.tar.gz` held in memory, for a download. Plain ustar: a 512-byte header and the content padded to 512 for each file.
// ponytail: names longer than 100 bytes are skipped rather than written with a long-name extension; the notes' names are short.
pub fn tar_gz(files: &[obsidian::NoteFile]) -> std::io::Result<Vec<u8>> {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    for (name, content) in files {
        let bytes = content.as_bytes();
        if name.len() > 100 {
            continue;
        }
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{:011o}", bytes.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        // The checksum is the sum of every header byte with the checksum field itself counted as spaces.
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h[155] = b' ';
        gz.write_all(&h)?;
        gz.write_all(bytes)?;
        gz.write_all(&vec![0u8; (512 - bytes.len() % 512) % 512])?;
    }
    gz.write_all(&[0u8; 1024])?;
    gz.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tar_gz_holds_each_file_under_its_name() {
        use std::io::Read;
        let files = vec![
            ("a/one.md".to_string(), "hello".to_string()),
            ("two.md".to_string(), "x".repeat(600)),
        ];
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(&tar_gz(&files).unwrap()[..])
            .read_to_end(&mut raw)
            .unwrap();
        // Entry one: header, then its 5 bytes padded to a block; entry two begins after that.
        assert_eq!(&raw[..8], b"a/one.md");
        assert_eq!(&raw[512..517], b"hello");
        assert_eq!(&raw[1024..1030], b"two.md");
        assert_eq!(&raw[1536..1546], "x".repeat(10).as_bytes());
        assert_eq!(raw.len() % 512, 0);
        assert!(raw.len() >= 512 * 5 + 1024);
    }
}
