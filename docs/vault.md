# Vault emulation

All three adapters use the emulator API port, `9000` by default.

| Service | Initial operations | Route selector |
| --- | --- | --- |
| AWS KMS | `CreateKey`, `DescribeKey`, `Encrypt`, `Decrypt`, `GenerateDataKey`, `GenerateDataKeyWithoutPlaintext` for symmetric keys | `X-Amz-Target: TrentService.<operation>` |
| Azure Key Vault | Set and get versioned secrets. Create and get RSA key versions. Encrypt, decrypt, wrap, and unwrap with `RSA-OAEP` or `RSA-OAEP-256` | Vault hostname or `/keys` and `/secrets` requests with `api-version` |
| Google Cloud KMS | Create and get key rings and symmetric keys. Get the initial key version. Encrypt and decrypt with optional additional authenticated data and CRC32C | Cloud KMS hostname or key-ring paths with a Google SDK client header. Add `X-Sqrzl-Vault: google-kms` for raw loopback requests. |

Use the AWS SDK with its endpoint set to the emulator URL. AWS and Google
ciphertexts are emulator-specific and use AES-256-GCM. Azure returns the public
part of a created RSA key as a JWK. The Google adapter uses the HTTP/JSON
transport. Azure SDKs can use a local TLS proxy and a fixture token for the
Bearer challenge. [AWS KMS API](https://docs.aws.amazon.com/kms/latest/APIReference/API_CreateKey.html),
[Azure Key Vault REST API](https://learn.microsoft.com/en-us/azure/key-vault/authentication-requests-and-responses),
[Google Cloud KMS REST API](https://docs.cloud.google.com/kms/docs/reference/rest)

AWS uses the emulator's existing SigV4 configuration. Azure and Google accept
local fixture bearer tokens through `SQRZL_AZURE_VAULT_TOKEN` and
`SQRZL_GOOGLE_KMS_TOKEN` respectively. Set these when using the vault adapters
with global authentication enabled. Azure and Google accept unsigned requests
when both their token and global authentication are disabled.

## Persistence

State starts in memory by default. For persistence, generate a key once and
supply the same value every time the emulator opens the same data directory.

```sh
export SQRZL_VAULT_KEY="$(openssl rand -base64 32)"
cargo run --locked --bin sqrzl-emulator
```

The emulator atomically replaces `$SQRZL_BLOBS_PATH/.sqrzl-vault` with an
AES-256-GCM snapshot. The encryption key is supplied through the environment.
Temporary snapshot files contain ciphertext. A saved vault requires its
original key to reopen. Use fixture keys and secrets for these local workflows.
