# Writing task pages

*The convention for how-to and tutorial steps that change the configuration.*

This page is for contributors. It describes how a task page in `docs/product/how-to/` or `docs/product/tutorials/` shows a configuration change, and it gives a skeleton to copy. The reference example is [`how-to/route-a-bucket-to-a-backend.md`](../product/how-to/route-a-bucket-to-a-backend.md). The reader-facing explanation of why there are two forms is [`explanation/two-ways-to-configure.md`](../product/explanation/two-ways-to-configure.md).

## When the convention applies

It applies to every step that changes the configuration of the proxy: a YAML section, a setting in the admin UI, an environment variable, an IAM user or group, or a job. It does not apply to reference pages, explanation pages, or tasks that only send S3 requests, run the CLI, or run `kubectl` or `docker`. Those pages keep their current shape.

## The shape of a configuration step

A task that changes the configuration has up to four parts, in this order.

1. In the admin UI. Numbered steps, one action per step. The step names the control in bold, exactly as the UI labels it (check the TSX, not your memory), and it names the route (`/_/admin/...`) when the step opens a page. Name a sidebar entry as **Group → Leaf**, with the group and leaf labels of `ADMIN_IA` in `demo/s3-browser/ui/src/components/adminNavigation.tsx`. Each step has one annotated screenshot that marks the control. One screenshot may serve two or three consecutive steps when it shows all of them; it then carries a numbered callout per step, and the numbers match the step numbers.
2. The same change in YAML. A heading with exactly this text, and a short link to `../explanation/two-ways-to-configure.md` in its first sentence. Then one complete `# validate` block for exactly the change that the UI steps made, with the fixed example cast. Say which file and which section it goes in, and how to apply it: a restart, `POST /_/api/admin/config/apply`, or `deltaglider_proxy config apply <file> --server <url>`. If an environment variable controls the setting, say so and show the variable.
3. The exceptions, stated first. When the UI cannot make the change (a YAML-only or CLI-only setting, or IAM in declarative mode), say so in one sentence at the top of the task and give only the declarative path. When only the UI can make it (for example an IAM user in `gui` mode), say so and leave out the YAML recap.
4. Verify. Unchanged from the page's current shape: commands or checks that prove the change works.

A page with several tasks repeats parts 1 and 2 per task only when the YAML differs in a way that a reader needs to see. Otherwise one recap after the last UI task covers all of them, as in the reference example.

## Skeleton

Copy this, replace every `<placeholder>`, and delete what does not apply.

````markdown
# How to <goal>

This guide shows you how to <goal in one sentence>. <One sentence on the mechanism, with a link to the explanation page that owns it.>

<Optional: one paragraph that introduces the example with the fixed cast, such as "The example routes downloads to local-disk.">

## 1. <First task>

In the admin UI:

1. In the sidebar, open **<Group> → <Leaf>** (`/_/admin/<leaf-path>`).
2. Click **<Button label>**.

   ![<Full sentence that says what callout 1 and callout 2 point at.>](/_/screenshots/<page-slug>-<shot>.webp)

3. In **<Field label>**, type `<value>`.

   ![<Full sentence that says what the arrow points at.>](/_/screenshots/<page-slug>-<shot>.webp)

4. Click **Review & apply**, check the diff, and then click **Apply and Persist**.

   ![<Full sentence that says what the arrow points at.>](/_/screenshots/<page-slug>-<shot>.webp)

<One paragraph on what the proxy does with the change, if the reader needs it: validation, probes, restart-only fields.>

## The same change in YAML

The steps above write this configuration into the `<section>` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
<section>:
  <key>: <value>
```

<If an environment variable controls the setting: "The environment variable `DGP_<NAME>` overrides this field. When it is set, the admin UI shows the field as read-only with a from env badge.">

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## Verify

1. <Check that proves the change is live.>

   ```bash
   <command>
   ```

## Related

- [<Page>](<path>): <what the reader finds there>.
````

For a YAML-only task, replace the "In the admin UI" part with one sentence, for example: "The admin UI cannot change this setting, so you set it in `deltaglider_proxy.yaml`." For a UI-only task, end the UI steps with one sentence, for example: "Users live in the encrypted config database in `gui` mode, so there is no YAML for this step. To manage users in YAML, see [How to manage IAM as code](../how-to/manage-iam-as-code.md)."

## YAML blocks

Every ```` ```yaml ```` block in `docs/product/` starts with exactly one marker line, and `scripts/check-docs-yaml-examples.sh` fails the build for a block without one:

- `# validate`: a complete proxy config document. The check runs `deltaglider_proxy config lint` on it. "The same change in YAML" is always a `# validate` block.
- `# fragment`: part of a proxy config, such as one section, one list item or a few lines of a backend, shown for reading. On a task page, the same content also appears in a full `# validate` block. Keep fragments short.
- `# not-proxy-config: <kind>`: a file that is not a proxy config, for example `# not-proxy-config: helm-values`, `kubernetes`, `compose` or `prometheus`.

A `# validate` block may use `${env:NAME}` references. The lint expands them and fails on an unset variable, so the check gives every referenced variable a placeholder value (64 hex characters for a name that contains `ENCRYPTION_KEY`, text otherwise). Keep secrets as references in the example; do not turn a block into a fragment only because it references a variable. The generated `changelog.md` is not checked.

## Screenshots

Screenshots come only from the screenshot pipeline, never from a manual capture. The pipeline boots a proxy with the fixed example cast, seeds it, and takes every shot in a light and a dark theme. The script is `scripts/docs-screenshots.sh`, and the shots are data: one file per docs area under `demo/s3-browser/ui/e2e/docs-screenshots/shots/<area>.ts`. A shot entry has this form:

```ts
{
  id: 'route-bucket-backend-select',
  route: '/_/admin/storage/buckets',
  setup: async (page) => { /* open the downloads row and its Backend list */ },
  annotations: [
    { target: 'the Backend select of the downloads row', kind: 'arrow', label: '' },
    { target: 'the local-disk option', kind: 'box', label: '' },
  ],
}
```

`kind` is `arrow`, `box` or `callout`. A `callout` carries a step number in `label`, and that number must match the step in the page. The `target` names the control by its visible label or its role, the same name that the step text uses.

To add a shot:

1. Pick the id: the page slug, a hyphen, and a short name of what the shot shows, for example `route-bucket-alias`. Use hyphens only.
2. Add the entry to the shot file of the area (storage, access, jobs, or observability and system). If you cannot write the setup yourself, describe it in the pull request or the plan: the route, the state to set up with the fixed cast, the control to point at by its label, and the step number.
3. Run `scripts/docs-screenshots.sh --update --only <id>`. It writes `docs/screenshots/<id>.light.webp` and `docs/screenshots/<id>.dark.webp`. Run one capture at a time on a machine (for example under `flock`), because every run uses MinIO on port 9000 and the fixed state directory `/tmp/dgp-docs`. With `--update`, the script rewrites every shot that it captures, so always name the shots with `--only`. Without `--update`, the script compares the capture with the committed files and calls a shot unchanged when fewer than 0.5 percent of its pixels differ. A small change, such as a moved annotation or a new help sentence, can stay under that limit: re-capture such a shot with `--update --only <id>` yourself, because the compare run does not flag it.
4. When a shot needs a server state that the seed cannot hold, because the other shots show the seed state, the shot changes the state in `setup` through the admin API and restores it in `teardown`. The runner calls `teardown` after each capture, also after a failure.
5. Reference the shot once, theme-neutral, as `![Full sentence.](/_/screenshots/<id>.webp)`. Both viewers pick the variant that matches the theme.

The folder has a budget, because the binary embeds `docs/screenshots/` and every byte is in every download: 150 KB per file and 12 MB for the folder (11.3 MB for 90 shots in September 2026). The script encodes WebP at quality 65 and steps down to 50 for a busy shot; at device scale 2, the text stays sharp. A shot that no page uses, or a file that the pipeline did not make, fails `scripts/check-docs-images.sh`. When a new shot does not fit, crop it with `clip` or replace an old one.

The alt text is a full sentence that ends with a period. It says what the arrow, the box or each numbered callout points at, because the viewers show it as the caption and a screen reader reads it. No version number and no build chip may be visible in a shot, because the screenshots are served to anonymous requests.

## Checklist

- Every step has one action, and every UI step has a screenshot or shares a numbered one.
- Every bold label matches the UI text, and every sidebar path matches `ADMIN_IA`.
- The YAML recap validates (`./scripts/check-docs-yaml-examples.sh`) and uses the fixed example cast.
- The heading "The same change in YAML" links to the explanation page.
- The prose follows the rules in the repo `CLAUDE.md`: full sentences, "requests" and not "calls", one idea per sentence, the mechanism before the consequence.
- `./scripts/check-docs-registry.sh` passes, and the manifest has an entry for a new page.
