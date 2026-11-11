//! What a commit says about itself: its author, committer, and message, and
//! its signature, read as the verdict on it needs ([`CommitFacts`]). The
//! commit is read with `git cat-file`, and an SSH signature is verified by
//! `git verify-commit`, which runs `ssh-keygen -Y verify` (rule 2: real tools
//! judge realness). Whether a signature verifies with the key it names
//! depends on the commit alone, so it is checked against that key, not
//! against the world's keys: which account holds the key is the world's to
//! say, under the write or read that asks.

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use orrery_world::{CommitFacts, CommitSignature};

use crate::GitFaceError;
use crate::store::{find_on_path, hermetic_git};

const SSH_ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const SSH_ARMOR_END: &str = "-----END SSH SIGNATURE-----";

/// A commit object's parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Ident,
    pub committer: Ident,
    pub message: String,
    /// The `gpgsig` header, its continuation lines joined with newlines, or
    /// `None` for a commit that carries none.
    pub signature: Option<String>,
    /// The commit with its signature header removed, which is what the
    /// signature signs.
    pub payload: String,
}

/// An author or committer line: a name, an address, and a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub email: String,
    pub unix: i64,
}

/// The commits named by `shas` that are commits in the repository, in the
/// order named. One `git cat-file --batch` reads them all.
pub fn read_commits(bare: &Path, shas: &[&str]) -> Result<Vec<Commit>, GitFaceError> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = hermetic_git(bare)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut names = String::new();
    for sha in shas {
        // A name with a leading `-`, or any space, is no object id; cat-file
        // reads a line as one name, so it answers "missing" for it.
        names.push_str(&sha.replace(['\n', ' '], ""));
        names.push('\n');
    }
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let writer = std::thread::spawn(move || stdin.write_all(names.as_bytes()));
    let out = child.wait_with_output()?;
    writer.join().expect("the writer does not panic")?;
    if !out.status.success() {
        return Err(GitFaceError::Git {
            args: "cat-file --batch".to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let mut rest = out.stdout.as_slice();
    let mut commits = Vec::new();
    while !rest.is_empty() {
        let header_end = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| GitFaceError::Cgi("cat-file --batch: a header with no end".into()))?;
        let header = String::from_utf8_lossy(&rest[..header_end]).into_owned();
        rest = &rest[header_end + 1..];
        let fields: Vec<&str> = header.split(' ').collect();
        let [sha, kind, size] = fields[..] else {
            // `<name> missing` or `<name> ambiguous`.
            continue;
        };
        let size: usize = size
            .parse()
            .map_err(|_| GitFaceError::Cgi(format!("cat-file --batch: header {header:?}")))?;
        let body = rest
            .get(..size)
            .ok_or_else(|| GitFaceError::Cgi("cat-file --batch: a short object".into()))?;
        rest = rest.get(size + 1..).unwrap_or_default();
        if kind == "commit" {
            commits.push(parse_commit(sha, body));
        }
    }
    Ok(commits)
}

/// The commit `sha` names, or `None` when it names no commit.
pub fn read_commit(bare: &Path, sha: &str) -> Result<Option<Commit>, GitFaceError> {
    Ok(read_commits(bare, &[sha])?.pop())
}

fn parse_commit(sha: &str, raw: &[u8]) -> Commit {
    let text = String::from_utf8_lossy(raw);
    let (headers, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let (mut tree, mut parents, mut author, mut committer) =
        (String::new(), Vec::new(), None, None);
    let mut signature: Option<String> = None;
    let mut payload_headers = Vec::new();
    let mut in_signature = false;
    for line in headers.split('\n') {
        if let Some(continued) = line.strip_prefix(' ') {
            if in_signature && let Some(signature) = signature.as_mut() {
                signature.push('\n');
                signature.push_str(continued);
                continue;
            }
            payload_headers.push(line);
            continue;
        }
        in_signature = false;
        let (name, value) = line.split_once(' ').unwrap_or((line, ""));
        match name {
            "gpgsig" => {
                signature = Some(value.to_string());
                in_signature = true;
                continue;
            }
            "tree" => tree = value.to_string(),
            "parent" => parents.push(value.to_string()),
            "author" => author = Some(Ident::parse(value)),
            "committer" => committer = Some(Ident::parse(value)),
            _ => {}
        }
        payload_headers.push(line);
    }
    Commit {
        sha: sha.to_string(),
        tree,
        parents,
        author: author.unwrap_or_else(Ident::missing),
        committer: committer.unwrap_or_else(Ident::missing),
        message: message.to_string(),
        signature,
        payload: format!("{}\n\n{message}", payload_headers.join("\n")),
    }
}

impl Ident {
    /// `Name <email> <unix> <zone>`, as git writes an ident. A commit made
    /// some other way still exists, so a part that does not parse is empty,
    /// or 0 for the time, rather than the commit going unread.
    fn parse(value: &str) -> Ident {
        let Some((open, close)) = value
            .rfind('<')
            .and_then(|open| Some((open, value[open..].find('>')? + open)))
        else {
            return Ident {
                name: value.trim().to_string(),
                email: String::new(),
                unix: 0,
            };
        };
        Ident {
            name: value[..open].trim_end().to_string(),
            email: value[open + 1..close].to_string(),
            unix: value[close + 1..]
                .split_ascii_whitespace()
                .next()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
        }
    }

    fn missing() -> Ident {
        Ident::parse("")
    }
}

/// What the verdict on `commit`'s signature reads from the commit. An SSH
/// signature is verified with `git verify-commit`, which needs `ssh-keygen`
/// on `PATH`; one that cannot be found is [`GitFaceError::NoSshKeygen`].
pub fn commit_facts(bare: &Path, commit: &Commit) -> Result<CommitFacts, GitFaceError> {
    let signature = match commit.signature.as_deref() {
        None => CommitSignature::Unsigned,
        Some(armored) if armored.starts_with(SSH_ARMOR_BEGIN) => match ssh_signature_key(armored) {
            None => CommitSignature::MalformedSsh,
            Some(key) => CommitSignature::Ssh {
                verifies: verifies_with(bare, &commit.sha, &key)?,
                key,
            },
        },
        Some(_) => CommitSignature::Other,
    };
    Ok(CommitFacts {
        sha: commit.sha.clone(),
        committer_email: commit.committer.email.clone(),
        signature,
    })
}

/// The key an armored SSH signature names, as the world spells a key: the
/// public key in its `SSHSIG` blob. `None` when the armor or the blob does
/// not parse.
fn ssh_signature_key(armored: &str) -> Option<String> {
    let body = armored
        .strip_prefix(SSH_ARMOR_BEGIN)?
        .trim_end()
        .strip_suffix(SSH_ARMOR_END)?;
    let blob = STANDARD
        .decode(body.split_ascii_whitespace().collect::<String>())
        .ok()?;
    let rest = blob.strip_prefix(b"SSHSIG")?;
    let version = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
    if version != 1 {
        return None;
    }
    let rest = &rest[4..];
    let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    orrery_world::ssh_public_key_from_blob(rest.get(4..4 + len)?)
}

/// Whether `sha`'s signature verifies with `key`: `git verify-commit`
/// against an allowed-signers file holding that key alone, so it says
/// nothing about who holds the key. git hands `ssh-keygen` the commit's own
/// date as the time to verify at, and the file gives the key no validity
/// window, so the answer does not depend on when it is asked.
fn verifies_with(bare: &Path, sha: &str, key: &str) -> Result<bool, GitFaceError> {
    // git is handed the path rather than the bare name, so a missing program
    // is this error and never a signature that fails to verify.
    let ssh_keygen = find_on_path("ssh-keygen").ok_or(GitFaceError::NoSshKeygen)?;
    let mut allowed = tempfile::NamedTempFile::new()?;
    writeln!(allowed, "signer {key}")?;
    allowed.flush()?;
    let out = hermetic_git(bare)
        .arg("-c")
        .arg(format!("gpg.ssh.program={}", ssh_keygen.display()))
        .arg("-c")
        .arg(format!(
            "gpg.ssh.allowedSignersFile={}",
            allowed.path().display()
        ))
        .args(["verify-commit", sha])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(true),
        // verify-commit's answer for a signature that does not verify.
        Some(1) => Ok(false),
        _ => Err(GitFaceError::Git {
            args: format!("verify-commit {sha}"),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_commit, ssh_signature_key};

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2wv3";

    #[test]
    fn a_signed_commit_parses_into_its_parts_and_its_payload() {
        let raw = "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
                   parent 1111111111111111111111111111111111111111\n\
                   author Alice Liddell <alice@corp.test> 1700000000 +0100\n\
                   committer Bob <bob@corp.test> 1700000060 -0700\n\
                   gpgsig -----BEGIN SSH SIGNATURE-----\n \
                   U1NIU0lH\n \
                   -----END SSH SIGNATURE-----\n\
                   \n\
                   subject\n\nbody\n";
        let c = parse_commit("abc", raw.as_bytes());
        assert_eq!(c.parents, ["1111111111111111111111111111111111111111"]);
        assert_eq!(
            (
                c.author.name.as_str(),
                c.author.email.as_str(),
                c.author.unix
            ),
            ("Alice Liddell", "alice@corp.test", 1_700_000_000)
        );
        assert_eq!(c.committer.email, "bob@corp.test");
        assert_eq!(
            c.signature.as_deref(),
            Some("-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----")
        );
        assert_eq!(c.message, "subject\n\nbody\n");
        assert!(!c.payload.contains("gpgsig") && !c.payload.contains("U1NIU0lH"));
        assert!(
            c.payload
                .ends_with("committer Bob <bob@corp.test> 1700000060 -0700\n\nsubject\n\nbody\n")
        );
    }

    #[test]
    fn a_signature_names_the_key_in_its_blob() {
        // An SSHSIG blob: magic, version 1, then the public key.
        let key_blob = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            KEY.split(' ').nth(1).unwrap(),
        )
        .unwrap();
        let mut blob = b"SSHSIG".to_vec();
        blob.extend(1u32.to_be_bytes());
        blob.extend((key_blob.len() as u32).to_be_bytes());
        blob.extend(&key_blob);
        let armored = format!(
            "-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &blob)
        );
        assert_eq!(ssh_signature_key(&armored).as_deref(), Some(KEY));
        for malformed in [
            "-----BEGIN SSH SIGNATURE-----\nnot base64\n-----END SSH SIGNATURE-----",
            "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----",
            "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH",
        ] {
            assert_eq!(ssh_signature_key(malformed), None, "{malformed:?}");
        }
    }
}
