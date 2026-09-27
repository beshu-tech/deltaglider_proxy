# The DeltaGlider license

DeltaGlider Proxy is published under the Business Source License 1.1 (BUSL-1.1). This page explains what that license permits, where the paid boundary sits, and what happens to every release over time. It is a plain-English summary. The [license text itself](https://github.com/beshu-tech/deltaglider_proxy/blob/main/LICENSE) is short and readable, and it is the authoritative version.

## What you can do for free

The license grants free production use to any organization whose stored footprint stays at or under **15 terabytes** (15 × 10^12 bytes). Development, testing, and evaluation are always free, at any size. Copying, modifying, and redistributing the source are also permitted by the license.

The stored footprint is the number of bytes that all of your organization's production deployments of the proxy cause to be stored, measured after DeltaGlider's compression. It counts every stored copy that DeltaGlider writes: the primary data, the replicas and archives that DeltaGlider writes to other buckets or regions, and the reference and metadata objects that delta compression needs. It is not the size of your source data, and it is not traffic. Because delta compression typically shrinks versioned artifacts by a large factor, the footprint is usually much smaller than the source data.

The footprint does not include bytes that your storage provider stores for its own redundancy. The provider writes those bytes, not DeltaGlider, so they do not count. For example, S3 cross-region replication that you turn on at your provider does not add to your footprint, but a replication rule that DeltaGlider runs to a second region does.

The footprint is the average, over a calendar month, of one measurement a day. Because the grant looks at the monthly average, a short spike, such as a migration that briefly holds two copies of a bucket, raises the footprint only for the days that the extra copy exists.

Your organization is you together with every entity that controls you, that you control, or that is under common control with you. Control means direct or indirect ownership of more than 50% of the voting interests. A parent company and its subsidiaries therefore share one grant.

You can check which side of the line you are on at any time: the admin dashboard and the `/_/stats` endpoint of each deployment report the compressed bytes that the deployment stores. The software never phones home, never asks for a license key, and never gates a feature. The license is a legal term, and the software does not enforce it technically.

## When a commercial license is required

Four situations fall outside the free grant:

1. **Your organization's stored footprint exceeds 15 TB.** The license then requires the flat [Commercial plan](https://deltaglider.com/pricing/). The plan is priced per organization, so it covers any number of instances, clusters, and regions, up to a stored footprint of 1 PB.
2. **Your organization's stored footprint exceeds 1 PB, or you need custom terms** such as indemnity, a security review, or procurement paperwork. This is the Enterprise plan, which has no fixed price. Contact sales@beshu.tech.
3. **You offer DeltaGlider to third parties as a hosted, managed, or multi-tenant service.** This also applies to a service whose primary value derives from DeltaGlider. It needs an OEM license. Running the proxy for your own applications, including inside a SaaS product whose value lies elsewhere, is not this case. Running DeltaGlider for a single customer, inside infrastructure that the customer controls, is not this case either: the license treats it as that customer's own production use.
4. **You ship DeltaGlider inside or with a commercial product or appliance.** The free grant does not cover a copy that a customer received as part of a third party's commercial product, unless that third party holds an OEM license that covers the customer's use. The vendor therefore needs an OEM license, and that license covers its customers. Contact sales@beshu.tech.

## Every release becomes open source

The Business Source License carries a built-in conversion: two years after each specific version of DeltaGlider is first released, that version automatically becomes available under the Apache License 2.0, a permissive open-source license. The conversion is written into the license text, so it does not depend on any future decision by Beshu. Today's release is source-available; the same release is fully open source two years from now.

## Older releases stay GPL

Every release up to and including v1.17.0 was published under the GNU General Public License v3.0. Those releases remain under GPL-3.0 forever, because a published license grant cannot be withdrawn. The BUSL-1.1 terms apply to releases after v1.17.0. The license applies separately to each version, so releases v1.18.x and v1.19.x keep the Additional Use Grant text that they shipped with, and the terms on this page apply to later releases.

## Why this license

DeltaGlider's job is to reduce your storage bill. A cost-saving product is difficult to fund through goodwill alone, and the previous GPL license created no obligation for the way that people use the proxy: a standalone service that is never linked into other code and never redistributed. The BUSL structure keeps the product free for most users, keeps the entire source public for security review, guarantees that every version eventually becomes fully open source, and asks the organizations that save the most money to fund the engineering. The same model is used by MariaDB, CockroachDB, and Sentry, so procurement and legal teams have an established playbook for reviewing it.

## Related

- [Pricing](https://deltaglider.com/pricing/): the free grant, the Commercial plan, and the savings calculator
- [The LICENSE file](https://github.com/beshu-tech/deltaglider_proxy/blob/main/LICENSE): the authoritative text, including the exact grant wording
