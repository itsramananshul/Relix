# Credential Vault

Version: 0.4.1

The credential vault is a SQLite-backed, AES-256-GCM encrypted store for
secrets that agents need at runtime — API keys, OAuth refresh tokens, or
arbitrary named secrets. It is part of the coordinator node subsystem and is
disabled by default.

## Enabling the vault

```toml
[credentials]
enabled  = true
db_path  = "data/credentials.db"
```

Set `enabled = true` and provide a `db_path`. The six `credentials.*`
capabilities are only registered when `enabled = true`.

## Environment variable

```
RELIX_CREDENTIAL_KEY=<secret>
```

This is the master secret from which the AES-256-GCM key is derived via
Argon2id. It is consumed once at vault open; the derived key is held in
memory (zeroized on process exit). The variable name is the default; it can
be overridden with `master_key_env` in the config block.

## Encryption at rest

| Property | Value |
|---|---|
| Cipher | AES-256-GCM (`aes_gcm` crate) |
| Key size | 256 bits (32 bytes) |
| Nonce | 12 bytes of `OsRng` per encryption call |
| KDF | Argon2id (variant `Algorithm::Argon2id`, version `V0x13`) |
| KDF output | 32 bytes → used directly as AES-GCM key |
| KDF salt | 32 bytes of `OsRng`, stored in `vault_metadata`, fixed per vault |
| Storage format | `EncryptedValue { nonce_b64, ciphertext_b64 }` serialized as JSON |

The ciphertext includes the GCM authentication tag. A tampered ciphertext
fails decryption before any plaintext is returned.

## Argon2id KDF parameters

Default parameters used when no `[credentials]` block overrides them:

| Parameter | Default | Notes |
|---|---|---|
| `argon2_memory_cost` | `65536` KiB (64 MB) | `--m` in argon2 CLI |
| `argon2_time_cost` | `3` | iteration count |
| `argon2_parallelism` | `4` | lane count |

These parameters are stored in `vault_metadata` as a JSON blob
`{memory_cost, time_cost, parallelism, algorithm: "argon2id"}` at vault
creation. On every subsequent open the parameters are read back from metadata
— they do not come from the current config. Changing the config values has no
effect on an existing vault; you must run `relix credentials migrate-kdf` to
apply new parameters.

```toml
[credentials]
argon2_memory_cost  = 65536   # KiB; default 64 MB
argon2_time_cost    = 3       # iterations
argon2_parallelism  = 4       # lanes
```

## Vault format versions

| Version | KDF | Status |
|---|---|---|
| `1` | SHA-256, no salt | **Refused at open.** Run `relix credentials migrate-kdf`. |
| `2` | Argon2id + per-vault salt | Current. All new vaults. |

Opening a v1 vault returns `CredentialError::LegacyFormat`. The SHA-256
derivation path exists only inside the migration entry point and is
unreachable from the normal open path.

### KDF migration

To upgrade a legacy (v1) vault or to change Argon2id parameters:

```
relix credentials migrate-kdf \
    --db data/credentials.db \
    --new-key-version v2
```

The migration reads every row under the old KDF, re-encrypts under the new
Argon2id parameters with fresh nonces, verifies each round-trip in memory
before committing, and writes a sentinel audit row (`credential_id =
"__vault__"`, `event = "kdf_migrated"`) on success. A failed round-trip
verification rolls the entire transaction back and returns
`MigrationVerifyFailed`.

## Key versioning

Multiple named key versions allow zero-downtime key rotation:

```toml
[credentials.key_versions]
v1 = "RELIX_CREDENTIAL_KEY"    # original key (env var name)
v2 = "RELIX_CREDENTIAL_KEY_V2" # new key (env var name)
```

- The **active version** is the highest-ranked present entry (`v1 < v2 < … <
  v9 < v10`).
- New credentials are always encrypted under the active version.
- Existing rows decrypt under the `key_version` stored on the row.
- When `key_versions` is empty (the default), the vault uses an implicit
  `v1 → master_key_env` single-key mode.

### Rotating the vault key

Once a new key version is active in the config and its env var is set,
re-encrypt all rows with:

```
relix credentials rotate-vault-key
```

This runs a single atomic SQLite transaction: decrypts all rows under their
existing key version, re-encrypts under the active version with fresh nonces,
verifies each row before `COMMIT`, rolls back on any failure.

## Capabilities

| Method | Description |
|---|---|
| `credentials.store` | Encrypt and insert a new credential. |
| `credentials.get` | Decrypt and return. Enforces GATE 2 (see below). |
| `credentials.rotate` | Replace encrypted value, bump version column. |
| `credentials.revoke` | Flip `revoked = 1`. Revoked credentials are unreadable. |
| `credentials.list` | Return summaries — no encrypted blobs included. |
| `credentials.audit` | Return per-credential audit rows. |

### GATE 2: caller-equals-owner enforcement

`credentials.get` enforces that the caller's identity (`subject_id`) equals
the `owner_agent` stored on the credential row. There is **no** admin or
operator role bypass on this path. Credentials with no `owner_agent` are
denied by default — they are not a free-for-all readable by any caller.

If cross-owner access is required, it must be expressed as explicit policy on
a distinct capability; `credentials.get` will never grant it.

## Audit trail

Every mutating operation and every successful `credentials.get` writes an
`AuditEvent` row to the `credential_audit` table:

| Event | Trigger |
|---|---|
| `Stored` | New credential inserted |
| `Accessed` | Successful decryption via `credentials.get` |
| `Rotated` | Value replaced via `credentials.rotate` |
| `Revoked` | Row flipped revoked via `credentials.revoke` |
| `KdfMigrated` | KDF migration completed |

Retrieve audit rows with:

```
# via capability
credentials.audit{ name: "my-api-key", limit: 100 }
```

## In-memory hygiene

- Derived AES keys are stored in `BTreeMap<String, Zeroizing<[u8; 32]>>`;
  heap bytes are wiped on drop.
- Decrypted values are returned as `SecretString` (`Zeroizing<String>`
  newtype); heap bytes are wiped on drop.
- Caller seed arrays are zeroized immediately after the KDF `from_seed`
  call returns.
- `SecretString` implements constant-time equality (`subtle::ConstantTimeEq`).

## Rotation scheduler

A background `RotationScheduler` sweeps credentials on a configurable
interval and emits a `RotationNotification` for any non-revoked credential
whose `next_rotation_at_ms <= now`. The scheduler **does not auto-rotate**;
it signals. The coordinator can wire a `RotationNotifier` (default:
`LogRotationNotifier` which emits a `WARN` log line) to trigger external
rotation logic.

```toml
[credentials]
rotation_check_interval_secs = 60   # default; minimum enforced is 5
```

The spawn loop enforces `max(rotation_check_interval_secs, 5)` as the minimum
tick to prevent tight CPU loops.

## Configuration reference

```toml
[credentials]
enabled                    = false              # master switch
db_path                    = "data/creds.db"    # SQLite file path
master_key_env             = "RELIX_CREDENTIAL_KEY"  # default
rotation_check_interval_secs = 60              # scheduler tick
argon2_memory_cost         = 65536             # KiB (64 MB default)
argon2_time_cost           = 3
argon2_parallelism         = 4

[credentials.key_versions]
# v1 = "RELIX_CREDENTIAL_KEY"      # implicit when key_versions is empty
# v2 = "RELIX_CREDENTIAL_KEY_V2"   # set to activate multi-version mode
```

## Tenant isolation

When tenant isolation is enabled, credential operations filter by `tenant_id`.
The `store_for_tenant` / `get_for_tenant` / `list_for_tenant` methods add
`AND tenant_id = ?` filters. An empty `tenant_id` when tenant isolation is
enabled returns `MissingTenant`.

## Fail-closed conditions

| Condition | Error |
|---|---|
| Vault is v1 (legacy SHA-256) | `CredentialError::LegacyFormat`; must run `migrate-kdf` |
| Argon2 derivation fails | `CredentialError::Kdf`; vault refuses to open |
| `key_versions` empty and `RELIX_CREDENTIAL_KEY` unset | `CredentialError::NoActiveKeyVersion` |
| Unknown `key_version` on row | `CredentialError::UnknownKeyVersion` |
| `rotate-vault-key` round-trip mismatch | `CredentialError::MigrationVerifyFailed`; rolled back |
| Caller not `owner_agent` on `credentials.get` | Denied (no bypass) |
| `owner_agent` is `None` on `credentials.get` | Denied by default |

## SQLite schema (reference)

```sql
CREATE TABLE credentials (
    id                     TEXT PRIMARY KEY,    -- "cred_" + uuid
    name                   TEXT NOT NULL UNIQUE,
    value_encrypted        TEXT NOT NULL,        -- JSON {nonce_b64, ciphertext_b64}
    kind                   TEXT NOT NULL DEFAULT 'api_key',
    owner_agent            TEXT,
    created_at_ms          INTEGER NOT NULL,
    updated_at_ms          INTEGER NOT NULL,
    expires_at_ms          INTEGER,
    last_rotated_at_ms     INTEGER,
    rotation_interval_secs INTEGER,
    next_rotation_at_ms    INTEGER,
    revoked                INTEGER NOT NULL DEFAULT 0,
    revoked_at_ms          INTEGER,
    revoke_reason          TEXT,
    version                INTEGER NOT NULL DEFAULT 1,
    tenant_id              TEXT,
    key_version            TEXT
);
```

## See also

- [`approval-tokens.md`](approval-tokens.md) — Ed25519-signed approval tokens
- [`security.md`](security.md) — full security model and admission pipeline
- [`agents.md`](agents.md) — agent gate and ownership model
- [`configuration.md`](configuration.md) — full TOML reference
