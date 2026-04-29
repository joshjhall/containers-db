# Verification

Every `install_methods[]` entry in a tooldb version file declares a
`verification` block describing how its download should be authenticated.
The model is the four-tier ladder originally codified in the bash
`lib/base/checksum-verification.sh` family; the schema enforces that each
entry carries exactly the fields its tier needs and no fields belonging
to other tiers.

Source of truth for the bash implementation while it still ships:

- `containers/lib/base/checksum-verification.sh` — tier dispatcher
- `containers/lib/base/checksum-pinned.sh` — Tier 2
- `containers/lib/base/checksum-fetch.sh` — Tier 3
- `containers/lib/base/checksum-tier4.sh` — Tier 4 TOFU
- `containers/lib/base/gpg-verify*.sh`, `sigstore-verify.sh` — Tier 1

## The four tiers

### Tier 1 — Cryptographic signature (best)

Verifies the artifact against a publisher-published signature, either
GPG (detached `.asc` plus the publisher's public key) or sigstore
(cosign keyless, OIDC-issued certificate). Authenticity is proved end-to-end.

Use Tier 1 whenever the publisher offers it. Network MITM and registry
compromise both fail to produce a passing verification.

### Tier 2 — Pinned checksum (high)

A hex-encoded checksum baked directly into the tooldb version file.
Auditable via git review and changes only when a human (or the auto-patch
pipeline) updates the file. Resistant to live publisher compromise,
because the catalog SHA was committed before the compromise.

Use Tier 2 when the publisher does not sign releases but the catalog can
record a checksum at scan time.

### Tier 3 — Publisher-served checksum (medium)

The publisher serves a checksum file (e.g., `SHA256SUMS`) at install time.
Stops accidental corruption and registry-byte-swapping attacks but does
**not** stop a publisher whose release infrastructure has been compromised
— if the artifact is wrong, the checksum file usually is too.

Use Tier 3 when checksums are available from the publisher but signatures
are not, and pinning at scan time is impractical.

### Tier 4 — Trust-on-first-use (acceptable, with warning)

No upstream verification. luggage records the digest of whatever it
downloaded the first time and warns the operator. Subsequent installs
of the same version compare against that recorded digest.

Use Tier 4 only when every higher tier is genuinely unavailable.
Consumers MUST surface a security warning whenever a Tier 4 method is
selected; setting `tofu: true` is the explicit acknowledgment that you
read this paragraph.

## Field mapping

| Tier | Required                                                           | Forbidden                            | Algorithm enum         |
| ---- | ------------------------------------------------------------------ | ------------------------------------ | ---------------------- |
| 1    | `algorithm` ∈ {`gpg`, `sigstore`} + matching key/identity pair     | tier 2/3/4 carriers                  | `gpg`, `sigstore`      |
| 2    | `algorithm` ∈ {`sha256`, `sha512`}, `pinned_checksum`              | tier 1/3/4 carriers                  | `sha256`, `sha512`     |
| 3    | `algorithm` ∈ {`sha256`, `sha512`}, `checksum_url_template`        | tier 1/2/4 carriers                  | `sha256`, `sha512`     |
| 4    | `tofu: true`                                                       | tier 1/2/3 carriers                  | optional, hash only    |

The "matching key/identity pair" for Tier 1 is one of:

- GPG: `gpg_key_url` **and** `signature_url_template`
- sigstore: `sigstore_identity` **and** `sigstore_issuer`

`source_url_template`, `invoke`, `post_install`, `system_packages`, and
`platform` are orthogonal to the verification block and may appear with
any tier.

## Examples

Tier 1 (GPG):

```json
{
  "tier": 1,
  "algorithm": "gpg",
  "gpg_key_url": "https://example.invalid/release-key.asc",
  "signature_url_template": "https://example.invalid/{version}/artifact-{version}-{arch}.tar.gz.asc"
}
```

Tier 2 (pinned):

```json
{
  "tier": 2,
  "algorithm": "sha256",
  "pinned_checksum": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
}
```

Tier 3 (published):

```json
{
  "tier": 3,
  "algorithm": "sha256",
  "checksum_url_template": "https://example.invalid/{version}/SHA256SUMS"
}
```

Tier 4 (TOFU):

```json
{
  "tier": 4,
  "algorithm": "sha256",
  "tofu": true
}
```

Complete fixture documents for every tier live in
`fixtures/tier{1,2,3,4}-example.json`. Counter-examples that the schema
must reject live in `fixtures/_negative/`.
