# The DeltaGlider license

DeltaGlider Proxy is published under the Business Source License 1.1 (BUSL-1.1). This page explains what that license permits, where the paid boundary sits, and what happens to every release over time. It is a plain-English summary — the [license text itself](https://github.com/beshu-tech/deltaglider_proxy/blob/main/LICENSE) is short and readable, and it is the authoritative version.

## What you can do for free

The license grants free production use to any organization whose total compressed stored footprint stays at or under **15 terabytes**. The compressed stored footprint is the number of bytes physically stored on your backend by all of your production deployments of the proxy, measured after DeltaGlider's compression — not your source data size, and not traffic. Because delta compression typically shrinks versioned artifacts by a large factor, 15 TB of compressed footprint usually corresponds to hundreds of terabytes of source data.

Development, testing, and evaluation are always free, at any size. Copying, modifying, and redistributing the source are also permitted by the license.

You can check which side of the line you are on at any time: the admin dashboard and the `/_/stats` endpoint already report your compressed stored footprint. The software never phones home, never asks for a license key, and never gates a feature. The license is a legal term you can read, not a technical lock.

## When a commercial license is required

Three situations fall outside the free grant:

1. **Your compressed stored footprint exceeds 15 TB per organization.** At that scale DeltaGlider is typically saving you many times the plan price, and the license requires the flat [Commercial plan](https://deltaglider.com/pricing/).
2. **You offer DeltaGlider itself to third parties as a hosted or managed service.** Running the proxy for your own applications — including inside a SaaS product — is not this; selling access to DeltaGlider is.
3. **You embed DeltaGlider in a proprietary product you distribute.** This is an OEM arrangement — contact sales@beshu.tech.

## Every release becomes open source

The Business Source License carries a built-in conversion: two years after each specific version of DeltaGlider is first released, that version automatically becomes available under the Apache License 2.0, a permissive open-source license. The conversion is written into the license text, so it does not depend on any future decision by Beshu. Today's release is source-available; the same release is fully open source two years from now.

## Older releases stay GPL

Every release up to and including v1.17.0 was published under the GNU General Public License v3.0. Those releases remain under GPL-3.0 forever — a published license grant cannot be withdrawn. The BUSL-1.1 terms apply to releases after v1.17.0.

## Why this license

DeltaGlider's job is to reduce your storage bill. A cost-saving product is difficult to fund through goodwill alone, and the previous GPL license created no obligation for the way the proxy is actually used — a standalone service that is never linked into other code and never redistributed. The BUSL structure keeps the product free for the vast majority of users, keeps the entire source public for security review, guarantees that every version eventually becomes fully open source, and asks the deployments that save the most money to fund the engineering. The same model is used by MariaDB, CockroachDB, and Sentry, so procurement and legal teams have an established playbook for reviewing it.

## Related

- [Pricing](https://deltaglider.com/pricing/) — the free grant, the Commercial plan, and the savings calculator
- [The LICENSE file](https://github.com/beshu-tech/deltaglider_proxy/blob/main/LICENSE) — the authoritative text, including the exact grant wording
