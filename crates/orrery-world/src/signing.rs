//! Commit signing: the SSH keys accounts register for signing commits, and
//! the verdict on a commit's signature, which a push stores when it passes.
//!
//! The verdict reads two kinds of input. What a commit says about itself,
//! its committer address and its signature, and whether the signature
//! verifies with the key it names, is read from the commit by the git face
//! ([`CommitFacts`]): no write changes it. Which account holds that key, and
//! which addresses that account holds, is world state, read here under the
//! write or read that needs it.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::events::{self, Attribution, EventBody};
use crate::{
    CommitFacts, CommitSignature, CommitVerification, Result, SigningKey, Verdict, World,
    WorldError, log, objects, queries, uniqueness_violation,
};

/// The key types an account can register, OpenSSH's public key types that
/// sign. Certificates are not keys an account holds.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

const KEY_COLUMNS: &str = "id, actor_id, public_key, title, created_clock_unix";

/// A public key in OpenSSH's one-line format, `<type> <base64> [comment]`,
/// as its type and base64 blob with the comment dropped, which is how a
/// world stores and compares keys. `None` for anything else.
pub fn ssh_public_key(text: &str) -> Option<String> {
    let mut fields = text.split_ascii_whitespace();
    let kind = fields.next()?;
    let blob = STANDARD.decode(fields.next()?).ok()?;
    ssh_public_key_from_blob(&blob).filter(|key| key.split(' ').next() == Some(kind))
}

/// A public key's wire-format blob, such as an SSH signature carries, as
/// [`ssh_public_key`] spells a key. The blob's first field names its type.
pub fn ssh_public_key_from_blob(blob: &[u8]) -> Option<String> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    let kind = std::str::from_utf8(blob.get(4..4 + len)?).ok()?;
    let rest = &blob[4 + len..];
    let shaped = match kind {
        // A 32-byte key, length-prefixed.
        "ssh-ed25519" => rest.len() == 36 && rest[..4] == [0, 0, 0, 32],
        _ => KEY_TYPES.contains(&kind) && !rest.is_empty(),
    };
    shaped.then(|| format!("{kind} {}", STANDARD.encode(blob)))
}

impl World {
    /// Register `key` as one of the caller's signing keys. The key is
    /// stored without its comment, and one account holds it at most: a key
    /// any account already holds is refused.
    pub fn add_signing_key(
        &self,
        by: Attribution,
        key: &str,
        title: Option<&str>,
    ) -> Result<SigningKey> {
        let key =
            ssh_public_key(key).ok_or_else(|| WorldError::InvalidSigningKey(key.to_string()))?;
        self.write(by, |w| {
            let tx = w.tx;
            let stored = tx
                .query_row(
                    &format!(
                        "INSERT INTO ssh_signing_keys (actor_id, public_key, title, created_clock_unix)
                         VALUES (?1, ?2, ?3, ?4)
                         RETURNING {KEY_COLUMNS}"
                    ),
                    params![by.actor_id, key, title, queries::clock(tx)?],
                    row_to_key,
                )
                .map_err(|e| match uniqueness_violation(&e) {
                    true => WorldError::SigningKeyExists,
                    false => WorldError::from(e),
                })?;
            w.operation(EventBody::SshSigningKeyCreated(
                events::SshSigningKeyCreated {
                    subject: key_subject(tx, stored.id)?,
                    object: key_object(&stored),
                },
            ));
            Ok(stored)
        })
    }

    /// The signing keys an account holds, oldest first.
    pub fn signing_keys(&self, actor_id: i64) -> Result<Vec<SigningKey>> {
        let conn = self.lock();
        objects::list_by(
            &conn,
            &format!("SELECT {KEY_COLUMNS} FROM ssh_signing_keys WHERE actor_id = ?1 ORDER BY id"),
            actor_id,
            row_to_key,
        )
    }

    /// Delete one of the caller's signing keys. A key the caller does not
    /// hold is [`WorldError::NoSuchSigningKey`], whoever holds it. A verdict a
    /// push stored for a commit the key signed stays.
    pub fn delete_signing_key(&self, by: Attribution, id: i64) -> Result<()> {
        self.write(by, |w| {
            let tx = w.tx;
            let key = tx
                .query_row(
                    &format!(
                        "SELECT {KEY_COLUMNS} FROM ssh_signing_keys WHERE id = ?1 AND actor_id = ?2"
                    ),
                    params![id, by.actor_id],
                    row_to_key,
                )
                .optional()?
                .ok_or(WorldError::NoSuchSigningKey(id))?;
            let subject = key_subject(tx, id)?;
            tx.execute("DELETE FROM ssh_signing_keys WHERE id = ?1", params![id])?;
            w.operation(EventBody::SshSigningKeyDeleted(
                events::SshSigningKeyDeleted {
                    subject,
                    object: key_object(&key),
                },
            ));
            Ok(())
        })
    }

    /// When the push that brought `sha` into the repository stored a
    /// passing verdict for it, or `None` when it stored none.
    pub fn stored_commit_verification(&self, repo_id: i64, sha: &str) -> Result<Option<i64>> {
        stored(&self.lock(), repo_id, sha)
    }

    /// The verdict on a commit's signature: the one the push that brought it
    /// in stored, or else one reached now from `facts` and the world as it
    /// is. A push stores only a passing verdict, so a failing one is reached
    /// again on every read, and a commit whose fault the world no longer
    /// has, such as an address since verified, reads as verified.
    pub fn commit_verification(
        &self,
        repo_id: i64,
        facts: &CommitFacts,
    ) -> Result<CommitVerification> {
        let conn = self.lock();
        if let Some(at) = stored(&conn, repo_id, &facts.sha)? {
            return Ok(CommitVerification {
                verdict: Verdict::Verified,
                stored_clock_unix: Some(at),
            });
        }
        Ok(CommitVerification {
            verdict: verdict(&conn, facts)?.0,
            stored_clock_unix: None,
        })
    }
}

/// Store the verdict of each commit in `signed` that passes and has none
/// stored, inside a push's write, emitting `commit:verified` for each, in
/// `signed`'s order.
pub(crate) fn record_verifications(
    w: &mut log::Write<'_>,
    repo_id: i64,
    signed: &[CommitFacts],
) -> Result<()> {
    for facts in signed {
        if stored(w.tx, repo_id, &facts.sha)?.is_some() {
            continue;
        }
        let (Verdict::Verified, Some((key_id, signer_id))) = verdict(w.tx, facts)? else {
            continue;
        };
        let at = queries::clock(w.tx)?;
        w.tx.execute(
            "INSERT INTO commit_verifications (repo_id, sha, signer_id, key_id, verified_clock_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo_id, facts.sha, signer_id, key_id, at],
        )?;
        let (repo, owner) = objects::repo_refs(w.tx, repo_id)?;
        w.emit(EventBody::CommitVerified(events::CommitVerified {
            subject: events::CommitSubject {
                kind: events::CommitType::Tag,
                sha: facts.sha.clone(),
                repo,
                owner,
            },
            object: events::CommitVerificationObject {
                signer: objects::actor_ref(w.tx, signer_id)?,
                signing_key: events::SshSigningKeyRef { id: key_id },
                verified_unix: at,
            },
        }));
    }
    Ok(())
}

fn stored(conn: &Connection, repo_id: i64, sha: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT verified_clock_unix FROM commit_verifications WHERE repo_id = ?1 AND sha = ?2",
            params![repo_id, sha],
            |r| r.get(0),
        )
        .optional()?)
}

/// The verdict on `facts` in the world as `conn` holds it, and for a
/// signature an account's key made, that key's id and the account's. The
/// first that applies decides it: no signature; a signature by a key no
/// account holds, PGP and X.509 ones included; an SSH signature that does
/// not parse; one that does not verify; and then where the committer address
/// is held, the key's account first.
fn verdict(conn: &Connection, facts: &CommitFacts) -> Result<(Verdict, Option<(i64, i64)>)> {
    let (key, verifies) = match &facts.signature {
        CommitSignature::Unsigned => return Ok((Verdict::Unsigned, None)),
        CommitSignature::Other => return Ok((Verdict::UnknownKey, None)),
        CommitSignature::MalformedSsh => return Ok((Verdict::Malformed, None)),
        CommitSignature::Ssh { key, verifies } => (key, *verifies),
    };
    let Some((key_id, owner_id)) = conn
        .query_row(
            "SELECT id, actor_id FROM ssh_signing_keys WHERE public_key = ?1",
            params![key],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok((Verdict::UnknownKey, None));
    };
    let signer = Some((key_id, owner_id));
    if !verifies {
        return Ok((Verdict::BadSignature, signer));
    }
    // Addresses compare under the column's NOCASE collation.
    let owners: Option<bool> = conn
        .query_row(
            "SELECT verified FROM actor_emails WHERE actor_id = ?1 AND email = ?2",
            params![owner_id, facts.committer_email],
            |r| r.get(0),
        )
        .optional()?;
    let verdict = match owners {
        Some(true) => Verdict::Verified,
        Some(false) => Verdict::UnverifiedAddress,
        None => {
            let held: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM actor_emails WHERE email = ?1)",
                params![facts.committer_email],
                |r| r.get(0),
            )?;
            match held {
                true => Verdict::OtherAccountsAddress,
                false => Verdict::NoAccount,
            }
        }
    };
    Ok((verdict, signer))
}

fn key_subject(conn: &Connection, id: i64) -> Result<events::SshSigningKeySubject> {
    let owner_id: i64 = conn
        .query_row(
            "SELECT actor_id FROM ssh_signing_keys WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(WorldError::NoSuchSigningKey(id))?;
    Ok(events::SshSigningKeySubject {
        kind: events::SshSigningKeyType::Tag,
        id,
        owner: objects::actor_ref(conn, owner_id)?,
    })
}

fn key_object(key: &SigningKey) -> events::SshSigningKeyObject {
    events::SshSigningKeyObject {
        id: key.id,
        key: key.key.clone(),
        title: key.title.clone(),
        created_unix: key.created_clock_unix,
    }
}

fn row_to_key(r: &Row<'_>) -> rusqlite::Result<SigningKey> {
    Ok(SigningKey {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        key: r.get(2)?,
        title: r.get(3)?,
        created_clock_unix: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::{ssh_public_key, ssh_public_key_from_blob};

    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2wv3";

    #[test]
    fn a_key_is_its_type_and_blob_without_a_comment() {
        assert_eq!(
            ssh_public_key(&format!("{ED25519} alice@laptop")).as_deref(),
            Some(ED25519)
        );
        assert_eq!(
            ssh_public_key(&format!("  {ED25519}\n")).as_deref(),
            Some(ED25519)
        );
    }

    #[test]
    fn anything_else_is_not_a_key() {
        for text in [
            "",
            "ssh-ed25519",
            "ssh-ed25519 not-base64!",
            // The blob names another type than the line does.
            "ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2wv3",
            // An ed25519 key one byte short.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF1ZZZ9YNYg8yl9pzBjnLU8mjxuB7NQXuUV2YGVC2ws=",
            "-----BEGIN PGP PUBLIC KEY BLOCK-----",
        ] {
            assert_eq!(ssh_public_key(text), None, "{text:?}");
        }
        assert_eq!(ssh_public_key_from_blob(&[0, 0, 0, 200]), None);
    }
}
