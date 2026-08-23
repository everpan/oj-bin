//! 制品辅助：内容哈希命名与确定性 tgz（spec §2.8——mtime=0、权限抹平、无 uid）。

use std::path::Path;

/// SHA-256 前 16 hex。ponytail: 16 hex 碰撞在本地可信构建威胁模型下忽略（spec §7）。
pub fn hash16(data: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data.as_bytes());
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
}

/// src_dir 全部文件 → out 的 tar.gz；entry 路径 = `prefix/<相对路径>`，正斜杠。
/// 元数据抹平（mtime=0 / mode 0644 / uid=gid=0 / 空 uname）→ 同输入同字节。
pub fn write_tgz(src_dir: &Path, out: &Path, prefix: &str) -> Result<(), String> {
    let mut files = Vec::new();
    collect(src_dir, src_dir, &mut files)?; // 确定性排序（同 build_cmd::collect 风格）
    let file = std::fs::File::create(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    for rel in &files {
        let mut hdr = tar::Header::new_gnu();
        let data = std::fs::read(src_dir.join(rel)).map_err(|e| format!("read {}: {e}", rel.display()))?;
        hdr.set_size(data.len() as u64);
        hdr.set_mode(0o644);
        hdr.set_mtime(0);
        hdr.set_uid(0);
        hdr.set_gid(0);
        hdr.set_cksum();
        let name = format!("{prefix}/{}", rel.to_string_lossy().replace('\\', "/"));
        tar.append_data(&mut hdr, &name, data.as_slice())
            .map_err(|e| format!("tar append {name}: {e}"))?;
    }
    tar.into_inner().and_then(|g| g.finish().map(|_| ()))
        .map_err(|e| format!("finish tgz: {e}"))
}

fn collect(root: &Path, dir: &Path, acc: &mut Vec<std::path::PathBuf>) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            collect(root, &p, acc)?;
        } else {
            acc.push(p.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash16_stable_and_discriminating() {
        assert_eq!(hash16("x").len(), 16);
        assert_eq!(hash16("abc"), hash16("abc"));
        assert_ne!(hash16("abc"), hash16("abd"));
    }

    #[test]
    fn tgz_deterministic_and_prefixed() {
        let d = std::env::temp_dir().join(format!("oj-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let src = d.join("src/user-0.1.0");
        std::fs::create_dir_all(src.join("f")).unwrap();
        std::fs::write(src.join("routes.js"), "export default [];\n").unwrap();
        std::fs::write(src.join("f/api-abc.js"), "export default {};\n").unwrap();
        let a = d.join("a.tgz");
        let b = d.join("b.tgz");
        write_tgz(&src, &a, "user-0.1.0").unwrap();
        write_tgz(&src, &b, "user-0.1.0").unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap(), "同输入两次打包必须字节一致");
        // 解包验证前缀与元数据
        let f = std::fs::File::open(&a).unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
        let mut names = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            assert_eq!(e.header().mtime().unwrap(), 0);
            let n = e.path().unwrap().to_string_lossy().into_owned();
            assert!(n.starts_with("user-0.1.0/"), "{n}");
            names.push(n);
        }
        assert!(names.iter().any(|n| n == "user-0.1.0/routes.js"), "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
