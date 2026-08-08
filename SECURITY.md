# Security policy

DeltaGlider Proxy sits in the write path of your storage, so we treat
security reports with priority over feature work.

## Reporting a vulnerability

Please report vulnerabilities privately — do not open a public issue.

- Preferred: [GitHub private vulnerability reporting](https://github.com/beshu-tech/deltaglider_proxy/security/advisories/new)
- Email: contact@beshu.tech with the subject line `SECURITY: DeltaGlider`

Include what you can: affected version, configuration shape (backend
type, encryption mode, IAM mode), reproduction steps, and impact as you
understand it. We credit reporters in the advisory unless you ask us
not to.

## What to expect

- **Acknowledgement within 2 business days.** A human who works on the
  code reads the report — there is no triage vendor in between.
- **Assessment and severity within 5 business days**, using CVSS as a
  guide but judged against how the proxy is actually deployed.
- **Critical vulnerabilities** (remote compromise of the proxy, data
  disclosure across tenants or buckets, authentication bypass): we
  target a fix or a documented mitigation within **14 days**, released
  as a patch version. A GitHub security advisory is published once a
  fixed version is available.
- **Non-critical issues** are fixed in the next regular release and
  noted in the changelog.

Commercial-plan customers are notified directly by email when a
security release ships, ahead of the public advisory where responsible
disclosure allows it, and their reports fall under the plan's
business-hours response SLA.

## Supported versions

Security fixes land on the latest release line. Older versions do not
receive backports; upgrading is the supported path (the proxy is a
single binary and upgrades are drop-in within a major version).

## Verifying what you run

Every release ships with a SHA-256 checksum file, a CycloneDX/SPDX
SBOM, and Sigstore-backed build provenance. Verify a downloaded
artifact with:

```
gh attestation verify deltaglider_proxy-<target>.tar.gz \
  --repo beshu-tech/deltaglider_proxy
```

The SBOM (`sbom-spdx.json`) attached to each release lists every crate
in the binary, so your scanner can watch our dependency tree between
releases.
