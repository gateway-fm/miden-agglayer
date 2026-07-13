use anyhow::Context;
use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::{env, fs};

#[derive(Debug, Clone, Deserialize)]
pub struct AccountsConfig {
    pub service: AccountIdBech32,
    pub bridge: AccountIdBech32,
    /// Legacy field — kept for backward compatibility with existing TOML configs.
    /// New deployments use the dynamic faucet registry in the Store.
    #[serde(default)]
    pub faucet_eth: Option<AccountIdBech32>,
    /// Legacy field — kept for backward compatibility with existing TOML configs.
    #[serde(default)]
    pub faucet_agg: Option<AccountIdBech32>,
    /// Dedicated account for GER injection. Separate from `service` so the
    /// NTX builder's modifications to the service account don't cause stale
    /// state errors when submitting UpdateGerNotes.
    #[serde(default)]
    pub ger_manager: Option<AccountIdBech32>,
}

#[derive(Debug, Clone)]
pub struct AccountIdBech32(pub AccountId);

impl AccountIdBech32 {
    /// Render the account id as a bech32 string for the given network.
    ///
    /// Bech32 encodes the network in its HRP. Using the wrong HRP makes the
    /// address unfindable on the network's block explorer even though the
    /// underlying account is valid on-chain — see PR introducing this method.
    /// Any code that emits the bech32 form across a system boundary (on-disk
    /// config, logs, API responses, support tickets) MUST pass the
    /// `NetworkId` of the active node.
    pub fn to_bech32(&self, net_id: NetworkId) -> String {
        self.0.to_bech32(net_id)
    }
}

impl Display for AccountIdBech32 {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // Network-agnostic hex form. For the bech32 form (which encodes the
        // network HRP) call `to_bech32(net_id)` with an explicit NetworkId.
        write!(f, "{}", self.0.to_hex())
    }
}

impl<'de> Deserialize<'de> for AccountIdBech32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        let id = if s.starts_with("0x") || s.starts_with("0X") {
            AccountId::from_hex(&s).map_err(serde::de::Error::custom)?
        } else {
            // `from_bech32` validates the bech32 checksum but doesn't constrain the HRP;
            // older configs written before the network-id fix used the `mlcl` (local) HRP
            // even on testnet. Both forms decode to the same on-chain account.
            AccountId::from_bech32(&s)
                .map(|(_, id)| id)
                .map_err(serde::de::Error::custom)?
        };
        Ok(Self(id))
    }
}

/// On-disk representation. Holds the bech32 strings ready-encoded with the
/// network id supplied at save time, so the bech32 HRP matches the active
/// node. Constructing this from `AccountsConfig` is the only sanctioned way
/// to serialize the config — there is intentionally no derived `Serialize`
/// impl on `AccountsConfig` itself.
#[derive(Serialize)]
struct AccountsConfigOnDisk {
    service: String,
    bridge: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    faucet_eth: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    faucet_agg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ger_manager: Option<String>,
}

impl AccountsConfigOnDisk {
    fn from_config(config: &AccountsConfig, net_id: &NetworkId) -> Self {
        let enc = |id: &AccountIdBech32| id.to_bech32(net_id.clone());
        Self {
            service: enc(&config.service),
            bridge: enc(&config.bridge),
            faucet_eth: config.faucet_eth.as_ref().map(&enc),
            faucet_agg: config.faucet_agg.as_ref().map(&enc),
            ger_manager: config.ger_manager.as_ref().map(&enc),
        }
    }
}

/// Optional containment root for the store directory.
///
/// `--miden-store-dir` is an operator-supplied flag, so an absolute path is a
/// legitimate and required input — every deployment (Dockerfile, compose, the
/// e2e suite, the documented operator invocations) passes one, and the default
/// is the absolute `$HOME/.miden`. Rejecting absolute paths outright would
/// break all of them, so we do NOT.
///
/// Instead, defence-in-depth is opt-in: when `MIDEN_STORE_BASE` is set, the
/// resolved store directory MUST live inside it, so a store dir injected from
/// a less-trusted source (e.g. a templated env var) can't escape the area the
/// operator intended. Unset = unchanged behaviour.
fn store_base_from_env() -> Option<PathBuf> {
    env::var_os("MIDEN_STORE_BASE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Validate the store directory: reject `..` traversal (lexical and
/// post-symlink-resolution) and, when a containment `base` is configured,
/// reject any directory that escapes it. Absolute paths are allowed by design
/// (see [`store_base_from_env`]).
///
/// Canonicalize the longest existing ancestor of `dir` and re-append the
/// not-yet-created tail. Keeps a missing store dir in the same canonical
/// namespace as a canonicalized base so containment compares like-for-like
/// (macOS `/var` -> `/private/var`, or a symlinked mount on Linux). Best-effort:
/// returns `dir` unchanged when nothing can be canonicalized.
fn canonicalize_existing_prefix(dir: &Path) -> PathBuf {
    let Some(existing) = dir
        .ancestors()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
    else {
        return dir.to_path_buf();
    };
    match (existing.canonicalize(), dir.strip_prefix(existing)) {
        (Ok(canon), Ok(rel)) => canon.join(rel),
        _ => dir.to_path_buf(),
    }
}

fn sanitize_store_dir(dir: &PathBuf, base: Option<&Path>) -> anyhow::Result<PathBuf> {
    for component in dir.components() {
        if matches!(component, Component::ParentDir) {
            anyhow::bail!(
                "path traversal detected: store directory must not contain '..' \
                 components, got: {dir:?}"
            );
        }
    }

    // Canonicalize when the directory already exists on disk so that any
    // symlink-based traversal is also resolved. When it does not yet exist
    // (first run / --init), the lexical check above is sufficient.
    let resolved = if dir.exists() {
        let canonical = dir
            .canonicalize()
            .with_context(|| format!("failed to canonicalize store directory {dir:?}"))?;
        // After resolving symlinks, re-verify there are no `..` segments.
        for component in canonical.components() {
            if matches!(component, Component::ParentDir) {
                anyhow::bail!("path traversal detected after canonicalization: {canonical:?}");
            }
        }
        canonical
    } else {
        // Not yet on disk (first run / --init): resolve the existing-ancestor
        // prefix so the result shares the same canonical namespace as a
        // canonicalized base. Without this, a base that canonicalizes
        // differently from `dir` (macOS `/var` -> `/private/var`, or a
        // symlinked mount on Linux) makes a legitimate dir-inside-base look
        // like it escapes. The lexical `..` check above already ran.
        canonicalize_existing_prefix(dir)
    };

    // Opt-in containment: when MIDEN_STORE_BASE is configured the store dir
    // must live inside it. The `resolved` form above means this also catches
    // an existing dir that symlinks out of the base.
    if let Some(base) = base {
        let canonical_base = base.canonicalize().with_context(|| {
            format!("MIDEN_STORE_BASE {base:?} does not exist or is not accessible")
        })?;
        if !resolved.starts_with(&canonical_base) {
            anyhow::bail!(
                "store directory {resolved:?} escapes the configured MIDEN_STORE_BASE \
                 {canonical_base:?}"
            );
        }
    }

    Ok(resolved)
}

fn config_path(miden_store_dir_opt: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let miden_store_dir = miden_store_dir_opt.unwrap_or_else(|| {
        let current_dir = env::current_dir().unwrap_or(PathBuf::from("."));
        let base_dir = env::home_dir().unwrap_or(current_dir);
        base_dir.join(".miden")
    });
    let base = store_base_from_env();
    let safe_dir = sanitize_store_dir(&miden_store_dir, base.as_deref())?;
    Ok(safe_dir.join("bridge_accounts.toml"))
}

pub fn config_path_exists(miden_store_dir: Option<PathBuf>) -> anyhow::Result<bool> {
    let config_path = config_path(miden_store_dir)?;
    Ok(fs::exists(&config_path)?)
}

pub fn save_config(
    config: AccountsConfig,
    net_id: &NetworkId,
    miden_store_dir: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let on_disk = AccountsConfigOnDisk::from_config(&config, net_id);
    let config_toml = toml::to_string(&on_disk)?;
    let config_path = config_path(miden_store_dir)?;
    write_config_atomic(&config_path, &config_toml)?;
    Ok(config_path)
}

/// Atomically replace `config_path` with `config_toml`.
///
/// Writes to a sibling temp file in the same directory, fsync's it, then
/// renames it into place. The rename is atomic on POSIX when source and
/// destination are on the same filesystem — which is guaranteed here by
/// constructing the temp path with `with_extension` on the target path.
///
/// Motivation: a non-atomic `fs::write` can leave `bridge_accounts.toml`
/// truncated if the process is OOMKilled (or the host loses power) mid-write.
/// On the next start, `load_config` would silently deserialize the partial
/// file: fields like `ger_manager` are `#[serde(default)]` and would become
/// `None`, causing GER injection to fall back to the wrong account and fail
/// with `AccountDataNotFound`. Bali hit this failure mode after two OOMKills
/// in four days.
fn write_config_atomic(config_path: &std::path::Path, config_toml: &str) -> anyhow::Result<()> {
    // Sibling temp file on the same mount as the target — required for the
    // rename below to be atomic rather than degenerate to copy-then-unlink.
    let tmp = config_path.with_extension("toml.new");

    // Best-effort cleanup of any stragglers from a previously crashed run.
    // We don't care if it doesn't exist; only surface errors that aren't
    // NotFound so we don't mask the real failure below.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to clear stale temp file {}", tmp.display()));
        }
    }

    fs::write(&tmp, config_toml)
        .with_context(|| format!("failed to write temp config file {}", tmp.display()))?;

    // fsync the file contents before renaming so that a crash between the
    // write and the rename can't expose pre-fsync (zero-filled) contents
    // after the rename completes.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&tmp)
        .with_context(|| {
            format!(
                "failed to reopen temp config file {} for fsync",
                tmp.display()
            )
        })?;
    file.sync_all()
        .with_context(|| format!("failed to fsync temp config file {}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, config_path).with_context(|| {
        format!(
            "failed to rename temp config {} -> {}",
            tmp.display(),
            config_path.display()
        )
    })?;

    Ok(())
}

pub fn load_config(miden_store_dir: Option<PathBuf>) -> anyhow::Result<AccountsConfig> {
    let config_path = config_path(miden_store_dir)?;
    let config_toml = fs::read_to_string(config_path)?;
    let config = toml::from_str(&config_toml)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Valid protocol-0.15 (version-1) account id; the 0.14 v0 encoding is
    // rejected by the 0.15 codec (`UnknownAccountIdVersion`).
    const TEST_ACCOUNT_HEX: &str = "0xcc0000000000dd010000ee000000ff";

    fn dummy() -> AccountIdBech32 {
        AccountIdBech32(AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap())
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let bad = Some(PathBuf::from("/tmp/../etc"));
        let result = config_path(bad);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("path traversal"),
            "expected path traversal error, got: {msg}"
        );
    }

    #[test]
    fn rejects_embedded_traversal() {
        let bad = Some(PathBuf::from("/home/user/.miden/../../etc"));
        let result = config_path(bad);
        assert!(result.is_err());
    }

    /// Absolute paths are allowed BY DESIGN (Cantina MA#20): `--miden-store-dir`
    /// is an operator-supplied flag and every deployment passes an absolute
    /// path (`/var/lib/miden-agglayer-service`, the `$HOME/.miden` default,
    /// etc). Containment is the opt-in `MIDEN_STORE_BASE` mechanism, exercised
    /// by the `containment_*` tests below — not a blanket absolute-path ban.
    #[test]
    fn allows_clean_absolute_path() {
        let good = Some(PathBuf::from("/tmp/miden-test-store"));
        let result = config_path(good);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("bridge_accounts.toml"));
    }

    #[test]
    fn containment_allows_dir_inside_base() {
        let base = tempdir().unwrap();
        let inside = base.path().join("store");
        // Not-yet-created dir under the base is accepted (first-run / --init).
        let ok = sanitize_store_dir(&inside, Some(base.path()));
        assert!(ok.is_ok(), "dir under base must be allowed: {ok:?}");
    }

    #[test]
    fn containment_rejects_dir_outside_base() {
        let base = tempdir().unwrap();
        let outside = PathBuf::from("/var/lib/somewhere-else");
        let result = sanitize_store_dir(&outside, Some(base.path()));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes the configured"),
            "expected containment-escape error"
        );
    }

    #[test]
    fn containment_rejects_symlink_escape() {
        // A dir that exists but symlinks out of the base must be rejected once
        // canonicalized, even though its lexical path sits under the base.
        let base = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let link = base.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let result = sanitize_store_dir(&link, Some(base.path()));
        assert!(
            result.is_err(),
            "symlink escaping the base must be rejected; got {result:?}"
        );
    }

    #[test]
    fn no_base_allows_absolute_outside_any_root() {
        // With no containment configured, behaviour is unchanged: a clean
        // absolute path is accepted (only `..` / symlink-`..` are rejected).
        let result = sanitize_store_dir(&PathBuf::from("/var/lib/miden-agglayer-service"), None);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_parent_dir_traversal_with_base() {
        let base = tempdir().unwrap();
        let bad = base.path().join("..").join("etc");
        let result = sanitize_store_dir(&bad, Some(base.path()));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[test]
    fn allows_default_path() {
        let result = config_path(None);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.ends_with("bridge_accounts.toml"));
    }

    #[test]
    fn save_writes_bech32_with_configured_network_hrp() {
        let dir = tempdir().unwrap();
        let cfg = AccountsConfig {
            service: dummy(),
            bridge: dummy(),
            faucet_eth: Some(dummy()),
            faucet_agg: None,
            ger_manager: Some(dummy()),
        };
        save_config(cfg, &NetworkId::Testnet, Some(dir.path().to_path_buf())).unwrap();
        let body = fs::read_to_string(dir.path().join("bridge_accounts.toml")).unwrap();
        // Every emitted id must use the `mtst` testnet HRP — never `mlcl`.
        assert!(
            body.contains("\"mtst1"),
            "expected testnet (`mtst`) HRP in saved bech32; got:\n{body}"
        );
        assert!(
            !body.contains("\"mlcl1"),
            "saved bech32 must not use the local-network (`mlcl`) HRP; got:\n{body}"
        );
    }

    #[test]
    fn load_accepts_legacy_local_hrp_bech32() {
        // Files written by previous versions used the local-network HRP
        // unconditionally. They must still load — `from_bech32` ignores the HRP.
        let dir = tempdir().unwrap();
        let id = AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap();
        let local_hrp = NetworkId::new("mlcl").unwrap();
        let legacy_bech32 = id.to_bech32(local_hrp);
        let toml_body = format!("service = \"{b}\"\nbridge = \"{b}\"\n", b = legacy_bech32);
        fs::write(dir.path().join("bridge_accounts.toml"), toml_body).unwrap();
        let loaded = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(loaded.bridge.0, id);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let cfg = AccountsConfig {
            service: dummy(),
            bridge: dummy(),
            faucet_eth: None,
            faucet_agg: None,
            ger_manager: None,
        };
        save_config(cfg, &NetworkId::Testnet, Some(dir.path().to_path_buf())).unwrap();
        let loaded = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            loaded.bridge.0,
            AccountId::from_hex(TEST_ACCOUNT_HEX).unwrap()
        );
    }

    fn full_cfg() -> AccountsConfig {
        AccountsConfig {
            service: dummy(),
            bridge: dummy(),
            faucet_eth: Some(dummy()),
            faucet_agg: Some(dummy()),
            ger_manager: Some(dummy()),
        }
    }

    /// Simulates a mid-write OOMKill by truncating the on-disk file to half
    /// its length. `load_config` must surface the corruption as an error
    /// rather than deserialising a partial TOML — otherwise `#[serde(default)]`
    /// fields like `ger_manager` would silently fall back to `None`, which is
    /// the exact failure path that hurt Bali after two OOMKills in four days.
    /// After the corruption, a fresh `save_config` must heal the file.
    #[test]
    fn load_rejects_truncated_file_then_atomic_save_heals_it() {
        let dir = tempdir().unwrap();
        save_config(
            full_cfg(),
            &NetworkId::Testnet,
            Some(dir.path().to_path_buf()),
        )
        .unwrap();

        let path = dir.path().join("bridge_accounts.toml");
        let body = fs::read_to_string(&path).unwrap();
        // Truncate to half length — guaranteed to slice mid-key or mid-value
        // because every field name + bech32 value is longer than a few bytes.
        let half = body.len() / 2;
        fs::write(&path, &body[..half]).unwrap();

        let truncated_load = load_config(Some(dir.path().to_path_buf()));
        assert!(
            truncated_load.is_err(),
            "expected truncated TOML to fail to load; got Ok({:?})",
            truncated_load.ok()
        );

        // Re-save via the atomic path and confirm it loads cleanly with all
        // optional fields intact (i.e. not silently defaulted to None).
        save_config(
            full_cfg(),
            &NetworkId::Testnet,
            Some(dir.path().to_path_buf()),
        )
        .unwrap();
        let healed = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert!(
            healed.ger_manager.is_some(),
            "atomic save must round-trip ger_manager; got None which would silently \
             trigger the AccountDataNotFound regression"
        );
        assert!(healed.faucet_eth.is_some());
        assert!(healed.faucet_agg.is_some());
    }

    /// A stale `bridge_accounts.toml.new` left over from a crashed prior run
    /// must not block the next save. `write_config_atomic` should clean it up
    /// and rewrite cleanly.
    #[test]
    fn atomic_save_overwrites_stale_tmp_file_from_prior_crash() {
        let dir = tempdir().unwrap();
        let stale_tmp = dir.path().join("bridge_accounts.toml.new");
        fs::write(
            &stale_tmp,
            b"this is garbage left over from a crashed run\n",
        )
        .unwrap();
        assert!(stale_tmp.exists());

        save_config(
            full_cfg(),
            &NetworkId::Testnet,
            Some(dir.path().to_path_buf()),
        )
        .unwrap();

        // Temp file must be gone (consumed by the rename) and the real config
        // must load successfully.
        assert!(
            !stale_tmp.exists(),
            "stale temp file should have been removed or renamed away"
        );
        let loaded = load_config(Some(dir.path().to_path_buf())).unwrap();
        assert!(loaded.ger_manager.is_some());
    }

    /// Belt-and-braces: confirm the atomic save never leaves the target file
    /// in a partial state. After a successful save, the target's contents must
    /// exactly match what `toml::to_string` produced for the on-disk config.
    #[test]
    fn atomic_save_produces_complete_file_contents() {
        let dir = tempdir().unwrap();
        let cfg = full_cfg();
        let expected = toml::to_string(&AccountsConfigOnDisk::from_config(
            &cfg,
            &NetworkId::Testnet,
        ))
        .unwrap();
        save_config(cfg, &NetworkId::Testnet, Some(dir.path().to_path_buf())).unwrap();
        let actual = fs::read_to_string(dir.path().join("bridge_accounts.toml")).unwrap();
        assert_eq!(actual, expected);
    }
}
