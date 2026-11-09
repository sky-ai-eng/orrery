//! Repositories to serve, generated from a seed so a payload's name means the
//! same bytes on every machine: source-like text, incompressible binary
//! files, or both. Each is built once under the work directory and reused.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;

/// A payload's shape, parsed from its name: `tiny`, `src<MB>`, `bin<MB>`, or
/// both joined by `+` (`src168+bin150`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub source_mb: usize,
    pub binary_mb: usize,
}

impl Spec {
    pub fn parse(name: &str) -> Result<Spec> {
        let mut spec = Spec {
            source_mb: 0,
            binary_mb: 0,
        };
        if name == "tiny" {
            return Ok(spec);
        }
        for part in name.split('+') {
            let (field, mb) = if let Some(mb) = part.strip_prefix("src") {
                (&mut spec.source_mb, mb)
            } else if let Some(mb) = part.strip_prefix("bin") {
                (&mut spec.binary_mb, mb)
            } else {
                bail!("payload {name:?}: {part:?} is not src<MB> or bin<MB>");
            };
            *field = mb
                .parse()
                .with_context(|| format!("payload {name:?}: {part:?} has no size in MB"))?;
        }
        Ok(spec)
    }
}

/// A local repository holding `spec`, its `main` ready to push: one commit of
/// source text, then one of binary files, or a README alone for `tiny`.
pub fn build(work: &Path, name: &str, seed: u64) -> Result<PathBuf> {
    let spec = Spec::parse(name)?;
    let dir = work.join("payloads").join(format!("{name}-seed{seed}"));
    let built = dir.join(".git").join("orrery-bench-built");
    if built.exists() {
        return Ok(dir);
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    git::run(&dir, &["init", "-q", "-b", "main", "."])?;
    let mut rng = Rng::new(seed);
    if spec.source_mb > 0 {
        write_source(&dir.join("src"), spec.source_mb << 20, &mut rng)?;
        git::run(&dir, &["add", "-A", "."])?;
        git::run(&dir, &["commit", "-q", "-m", "source"])?;
    }
    if spec.binary_mb > 0 {
        write_binary(&dir.join("assets"), spec.binary_mb, &mut rng)?;
        git::run(&dir, &["add", "-A", "."])?;
        git::run(&dir, &["commit", "-q", "-m", "assets"])?;
    }
    if spec.source_mb == 0 && spec.binary_mb == 0 {
        std::fs::write(dir.join("README.md"), "# bench\n")?;
        git::run(&dir, &["add", "-A", "."])?;
        git::run(&dir, &["commit", "-q", "-m", "tiny"])?;
    }
    std::fs::write(built, "")?;
    Ok(dir)
}

/// `mb` files of one MiB of random bytes each.
pub fn write_binary(dir: &Path, mb: usize, rng: &mut Rng) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut buf = vec![0u8; 1 << 20];
    for i in 0..mb {
        rng.fill(&mut buf);
        std::fs::write(dir.join(format!("blob{i:04}.bin")), &buf)?;
    }
    Ok(())
}

/// About `bytes` of source-like text: lines of keywords, punctuation, and a
/// few hundred identifiers, in files of 2 to 64 KiB, 64 to a directory. It
/// compresses about as well as real source does, which is what decides how
/// much work a clone of it is.
fn write_source(root: &Path, bytes: usize, rng: &mut Rng) -> Result<()> {
    let words = vocabulary(rng);
    let mut written = 0;
    let mut file = 0;
    while written < bytes {
        let size = ((2048.0 * 2f64.powf(rng.unit() * 5.0)) as usize).min(bytes - written);
        let mut text = String::with_capacity(size + 128);
        while text.len() < size {
            for _ in 0..rng.below(5) {
                text.push_str("    ");
            }
            for t in 0..2 + rng.below(9) {
                if t > 0 {
                    text.push(' ');
                }
                text.push_str(&words[rng.below(words.len() as u64) as usize]);
            }
            text.push_str([";", " {", "", ","][rng.below(4) as usize]);
            text.push('\n');
        }
        let dir = root.join(format!("d{:03}", file / 64));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("f{file:05}.rs")), &text)?;
        written += text.len();
        file += 1;
    }
    Ok(())
}

fn vocabulary(rng: &mut Rng) -> Vec<String> {
    const FIXED: &[&str] = &[
        "fn", "let", "mut", "pub", "use", "impl", "struct", "enum", "match", "if", "else", "for",
        "in", "while", "return", "self", "Self", "&", "&mut", "->", "=>", "=", "==", "!=", "+",
        "-", "*", "?", "(", ")", "[", "]", "{", "}", "::", ".", "Ok(())", "Err(e)", "Some(x)",
        "None", "true", "false", "0", "1", "u64", "usize", "String", "Vec", "Option", "Result",
    ];
    const SYLLABLES: &[&str] = &[
        "re", "ad", "par", "se", "con", "fig", "buf", "fer", "lo", "ad", "st", "ate", "in", "dex",
        "ref", "map", "tree", "node", "log", "seq", "pull", "push", "head", "base", "tip", "pack",
    ];
    let mut words: Vec<String> = FIXED.iter().map(|w| w.to_string()).collect();
    for _ in 0..400 {
        let mut word = String::new();
        for s in 0..2 + rng.below(2) {
            if s > 0 && rng.below(3) == 0 {
                word.push('_');
            }
            word.push_str(SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize]);
        }
        words.push(word);
    }
    words
}

/// SplitMix64: small, fast, and the same sequence for a seed everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`, for `n` far below 2^64.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_names_parse() {
        let spec = |s, b| Spec {
            source_mb: s,
            binary_mb: b,
        };
        assert_eq!(Spec::parse("tiny").unwrap(), spec(0, 0));
        assert_eq!(Spec::parse("src40").unwrap(), spec(40, 0));
        assert_eq!(Spec::parse("src168+bin150").unwrap(), spec(168, 150));
        assert!(Spec::parse("src").is_err());
        assert!(Spec::parse("huge").is_err());
    }
}
