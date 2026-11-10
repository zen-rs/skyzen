//! `[static_files] precompress`: brotli, zstd and gzip siblings for every file a `StaticDir`
//! serves, written by `skyzen build` so the server never compresses on a request.
//!
//! Each coding runs at the strongest setting a browser still decodes — brotli quality 11 with the
//! default 4 MiB window, zstd level 19 (whose window stays inside the 8 MiB that RFC 8878 asks a
//! decoder to accept; the `--ultra` levels above it do not), gzip level 9 — because the work is
//! paid once per build and saved on every response. A sibling that is not smaller than its file is
//! not written, and a stale one is removed; one newer than its file is left alone, so a second
//! build over an unchanged tree compresses nothing.

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::SystemTime,
};

use anyhow::{Context, Result};

use crate::{output, providers::Task};

/// The codings, as the file suffix the server looks for.
const CODINGS: [Coding; 3] = [Coding::Brotli, Coding::Zstd, Coding::Gzip];

#[derive(Debug, Clone, Copy)]
enum Coding {
    Brotli,
    Zstd,
    Gzip,
}

impl Coding {
    const fn extension(self) -> &'static str {
        match self {
            Self::Brotli => "br",
            Self::Zstd => "zst",
            Self::Gzip => "gz",
        }
    }

    fn compress(self, source: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Brotli => {
                let params = brotli::enc::BrotliEncoderParams {
                    quality: 11,
                    size_hint: source.len(),
                    ..Default::default()
                };
                let mut out = Vec::new();
                brotli::BrotliCompress(&mut &source[..], &mut out, &params)?;
                Ok(out)
            }
            Self::Zstd => zstd::bulk::compress(source, 19),
            Self::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
                encoder.write_all(source)?;
                encoder.finish()
            }
        }
    }

    /// Whether `path` is already one of the siblings, which is never compressed again.
    fn is_sibling(path: &Path) -> bool {
        path.extension()
            .is_some_and(|extension| CODINGS.iter().any(|coding| extension == coding.extension()))
    }
}

/// Precompress every file below `directories`.
#[derive(Debug)]
pub struct Precompress {
    directories: Vec<PathBuf>,
}

impl Precompress {
    /// The directories, each resolved against `root` — the directory holding `Skyzen.toml`.
    pub fn new(root: &Path, directories: &[PathBuf]) -> Self {
        Self {
            directories: directories
                .iter()
                .map(|directory| root.join(directory))
                .collect(),
        }
    }
}

/// What one file's siblings needed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Outcome {
    written: usize,
    current: usize,
}

impl Task for Precompress {
    fn describe(&self) -> String {
        let directories: Vec<String> = self
            .directories
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();
        format!("precompress {} (br, zstd, gzip)", directories.join(", "))
    }

    fn run(&self) -> Result<()> {
        let mut files = Vec::new();
        for directory in &self.directories {
            anyhow::ensure!(
                directory.is_dir(),
                "[static_files] precompress names {}, which is not a directory",
                directory.display()
            );
            for entry in ignore::WalkBuilder::new(directory)
                .standard_filters(false)
                .build()
            {
                let entry =
                    entry.with_context(|| format!("failed to walk {}", directory.display()))?;
                if entry.file_type().is_some_and(|kind| kind.is_file())
                    && !Coding::is_sibling(entry.path())
                {
                    files.push(entry.into_path());
                }
            }
        }

        let outcome = compress_all(&files)?;
        output::ok(format!(
            "precompressed {} files: {} siblings written, {} already current",
            files.len(),
            outcome.written,
            outcome.current
        ));
        Ok(())
    }
}

/// Compress `files` on one worker per available core; brotli at quality 11 is slow enough that a
/// bundle's worth of files is CPU-bound.
fn compress_all(files: &[PathBuf]) -> Result<Outcome> {
    let workers = thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(files.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Vec<Result<Outcome>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut total = Outcome::default();
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let outcome = compress_file(file)
                            .with_context(|| format!("failed to precompress {}", file.display()))?;
                        total.written += outcome.written;
                        total.current += outcome.current;
                    }
                    Ok(total)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a precompression worker panicked"))
            .collect()
    });

    let mut total = Outcome::default();
    for result in results {
        let outcome = result?;
        total.written += outcome.written;
        total.current += outcome.current;
    }
    Ok(total)
}

fn compress_file(path: &Path) -> Result<Outcome> {
    let source = std::fs::read(path)?;
    let modified = std::fs::metadata(path)?.modified()?;
    let mut outcome = Outcome::default();
    for coding in CODINGS {
        let sibling = sibling(path, coding);
        if is_current(&sibling, modified)? {
            outcome.current += 1;
            continue;
        }
        let compressed = coding.compress(&source)?;
        if compressed.len() >= source.len() {
            // Not worth sending; a sibling left from an older, more compressible version would be
            // stale.
            match std::fs::remove_file(&sibling) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            continue;
        }
        std::fs::write(&sibling, compressed)
            .with_context(|| format!("failed to write {}", sibling.display()))?;
        outcome.written += 1;
    }
    Ok(outcome)
}

/// Whether `sibling` exists and was written no earlier than its file last changed.
fn is_current(sibling: &Path, source_modified: SystemTime) -> Result<bool> {
    match std::fs::metadata(sibling) {
        Ok(metadata) => Ok(metadata.modified()? >= source_modified),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn sibling(path: &Path, coding: Coding) -> PathBuf {
    let mut sibling = path.as_os_str().to_owned();
    sibling.push(".");
    sibling.push(coding.extension());
    PathBuf::from(sibling)
}

#[cfg(test)]
mod tests {
    use super::{Precompress, Task as _};
    use std::{io::Read as _, path::Path};

    /// Chinese, Japanese and Korean, repeated so every coding has something to remove.
    fn cjk_text() -> String {
        "你好，世界。こんにちは、世界。안녕하세요, 세계.\n".repeat(200)
    }

    fn decoded(path: &Path, extension: &str) -> Vec<u8> {
        let compressed = std::fs::read(path.with_file_name(format!(
            "{}.{extension}",
            path.file_name().unwrap().to_string_lossy()
        )))
        .unwrap();
        let mut out = Vec::new();
        match extension {
            "br" => {
                brotli::Decompressor::new(&compressed[..], 4096)
                    .read_to_end(&mut out)
                    .unwrap();
            }
            "zst" => out = zstd::stream::decode_all(&compressed[..]).unwrap(),
            "gz" => {
                flate2::read::GzDecoder::new(&compressed[..])
                    .read_to_end(&mut out)
                    .unwrap();
            }
            other => panic!("no coding {other}"),
        }
        out
    }

    #[test]
    fn every_file_gets_siblings_that_decode_to_it() {
        let root = tempfile::tempdir().unwrap();
        let public = root.path().join("public");
        std::fs::create_dir_all(public.join("资源")).unwrap();
        let script = public.join("资源").join("アプリ.js");
        let style = public.join("스타일.css");
        std::fs::write(&script, cjk_text()).unwrap();
        std::fs::write(&style, cjk_text().to_uppercase()).unwrap();

        Precompress::new(root.path(), &["public".into()])
            .run()
            .unwrap();

        for file in [&script, &style] {
            let original = std::fs::read(file).unwrap();
            for extension in ["br", "zst", "gz"] {
                assert_eq!(
                    decoded(file, extension),
                    original,
                    "{} .{extension}",
                    file.display()
                );
            }
        }
    }

    #[test]
    fn an_incompressible_file_gets_no_sibling_and_siblings_are_not_recompressed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("한.txt"), "가").unwrap();
        std::fs::write(root.path().join("big.txt"), cjk_text()).unwrap();

        let task = Precompress::new(root.path(), &[".".into()]);
        task.run().unwrap();
        task.run().unwrap();

        let mut names: Vec<String> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "big.txt",
                "big.txt.br",
                "big.txt.gz",
                "big.txt.zst",
                "한.txt"
            ],
            "one byte of Korean cannot shrink, and a second run must not compress `.br` files"
        );
    }

    #[test]
    fn a_changed_file_has_its_siblings_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("索引.html");
        std::fs::write(&file, cjk_text()).unwrap();
        let task = Precompress::new(root.path(), &[".".into()]);
        task.run().unwrap();

        // Make the siblings older than the new content, as a later edit would.
        let long_ago = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        for extension in ["br", "zst", "gz"] {
            std::fs::File::options()
                .write(true)
                .open(root.path().join(format!("索引.html.{extension}")))
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        let changed = format!("<h1>更新</h1>{}", cjk_text());
        std::fs::write(&file, &changed).unwrap();
        task.run().unwrap();

        for extension in ["br", "zst", "gz"] {
            assert_eq!(
                decoded(&file, extension),
                changed.as_bytes(),
                ".{extension}"
            );
        }
    }

    #[test]
    fn a_missing_directory_is_an_error_naming_it() {
        let root = tempfile::tempdir().unwrap();
        let error = Precompress::new(root.path(), &["dist".into()])
            .run()
            .unwrap_err();
        assert!(error.to_string().contains("dist"), "{error}");
    }
}
