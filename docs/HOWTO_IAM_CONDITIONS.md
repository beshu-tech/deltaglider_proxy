# How-To: Advanced IAM with Conditions

A step-by-step guide to using IAM policy conditions for fine-grained access control. Conditions let you restrict **where** and **how** users can access data, beyond just what actions they can perform.

## Prerequisites

- DeltaGlider Proxy with authentication enabled (see [Security Basics](HOWTO_SECURITY_BASICS.md))
- At least one IAM user created via the admin GUI
- Basic understanding of AWS IAM concepts (Allow/Deny, actions, resources)

---

## What Are Conditions?

Standard IAM permissions answer: **"Can user X do action Y on resource Z?"**

Conditions add context: **"...but only if the request comes from this IP range"** or **"...but only if the prefix matches this pattern."**

```
┌──────────────────────────────────────────────────┐
│  Permission Rule                                  │
│                                                   │
│  Effect:    Allow                                 │
│  Actions:   read, list                            │
│  Resources: my-bucket/*                           │
│                                                   │
│  Conditions:                                      │
│    ├── aws:SourceIp = 10.0.0.0/8                 │
│    └── s3:prefix NOT LIKE "internal/*"            │
│                                                   │
│  = "Allow read/list on my-bucket, but only from  │
│    the 10.x.x.x network, and not on the          │
│    internal/ prefix"                              │
└──────────────────────────────────────────────────┘
```

---

## Step 1: IP-Based Access Restrictions

**Scenario:** Your CI pipeline should only access the proxy from your build servers. If a CI credential leaks, attackers outside your network can't use it.

### Create the rule

In the admin GUI, edit the CI user's permissions:

```
Effect:    Allow
Actions:   read, write, list
Resources: builds-bucket/*
Conditions:
  IpAddress:
    aws:SourceIp: "10.0.0.0/8"
```

**How it works:**

```
  Request from 10.0.1.50 (build server)
    → aws:SourceIp = 10.0.1.50
    → IpAddress condition: 10.0.1.50 matches 10.0.0.0/8 ✅
    → Access granted

  Request from 203.0.113.42 (attacker with leaked key)
    → aws:SourceIp = 203.0.113.42
    → IpAddress condition: 203.0.113.42 does NOT match 10.0.0.0/8 ❌
    → Access denied (even with valid credentials!)
```

### Multiple IP ranges

```json
{
  "IpAddress": {
    "aws:SourceIp": ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
  }
}
```

### Important: Proxy Headers

For IP conditions to work behind a reverse proxy, the proxy must forward client IPs:

```bash
# DeltaGlider trusts X-Forwarded-For by default
DGP_TRUST_PROXY_HEADERS=true
```

Your reverse proxy must set `X-Forwarded-For`:
```nginx
proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
```

**Warning:** If `DGP_TRUST_PROXY_HEADERS=true` and the proxy is exposed directly to the internet, clients can spoof their IP by sending a fake `X-Forwarded-For` header, bypassing IP conditions entirely.

---

## Step 2: Prefix-Based List Restrictions

**Scenario:** A user should be able to list objects in their own prefix but not see other users' files.

### Create the rule

```
Effect:    Allow
Actions:   read, write, list, delete
Resources: shared-bucket/user-alice/*

Effect:    Deny
Actions:   list
Resources: shared-bucket/*
Conditions:
  StringNotLike:
    s3:prefix: "user-alice/*"
```

**How it works:**

```
  Alice calls: LIST shared-bucket?prefix=user-alice/docs/
    → s3:prefix = "user-alice/docs/"
    → Allow rule matches: resource "shared-bucket/user-alice/*" ✅
    → Deny condition: "user-alice/docs/" LIKE "user-alice/*" → condition NOT met
    → Deny rule does NOT apply
    → Result: ALLOWED ✅

  Alice calls: LIST shared-bucket?prefix=user-bob/
    → s3:prefix = "user-bob/"
    → Allow rule: resource "shared-bucket/user-alice/*" does NOT match ❌
    → Deny rule: resource "shared-bucket/*" matches
    → Deny condition: "user-bob/" NOT LIKE "user-alice/*" → condition MET
    → Result: DENIED ❌
```

### Deny listing of hidden files

Prevent any user from listing files starting with `.` (dotfiles):

```
Effect:    Deny
Actions:   list
Resources: *
Conditions:
  StringLike:
    s3:prefix: ".*"
```

---

## Step 3: Combining Conditions

Conditions within a single rule are ANDed (all must match). Multiple rules are evaluated independently with Deny taking precedence.

### Example: Geo-restricted read-only access

```
┌─────────────────────────────────────────────────────┐
│  Rule 1: Allow                                       │
│    Actions:   read, list                             │
│    Resources: public-bucket/*                        │
│    Conditions:                                       │
│      IpAddress:                                      │
│        aws:SourceIp: "198.51.100.0/24"   (office)   │
│                                                      │
│  Rule 2: Deny                                        │
│    Actions:   write, delete                          │
│    Resources: *                                      │
│    (no conditions — always applies)                  │
└─────────────────────────────────────────────────────┘

Evaluation:
  PUT from office IP   → Rule 1 doesn't cover "write"
                       → Rule 2 explicitly denies "write"
                       → DENIED ❌

  GET from office IP   → Rule 1 allows "read" + IP matches
                       → Rule 2 doesn't cover "read"
                       → ALLOWED ✅

  GET from home IP     → Rule 1: IP doesn't match
                       → No Allow → implicit deny
                       → DENIED ❌
```

---

## Step 4: Using Groups for Shared Policies

**Scenario:** Multiple users need the same permissions. Instead of duplicating rules, create a group.

### Create a group

1. Admin GUI → **Groups** tab → **Create Group**
2. Name: `ci-builders`
3. Permissions:

```
Effect:    Allow
Actions:   read, write, list
Resources: builds-bucket/*
Conditions:
  IpAddress:
    aws:SourceIp: "10.0.0.0/8"
```

4. Add users: `ci-user-1`, `ci-user-2`, `ci-user-3`

**How group permissions work:**

```
  User's effective permissions = User permissions ∪ Group permissions

  ┌─────────────────────┐     ┌─────────────────────┐
  │  User: ci-user-1     │     │  Group: ci-builders  │
  │  Permissions: (none)  │  +  │  Allow read,write    │
  │                       │     │  on builds-bucket/*  │
  │                       │     │  if IP in 10.0.0.0/8 │
  └─────────────────────┘     └─────────────────────┘
                    ↓
       Effective: Allow read,write on builds-bucket/*
                  if IP in 10.0.0.0/8
```

**Explicit Deny always wins:**

If either the user or any of their groups has a Deny rule, it overrides Allow rules from any source:

```
  User has: Allow * on *
  Group has: Deny delete on production-bucket/*

  → User can do everything EXCEPT delete from production-bucket/
```

---

## Step 5: Testing Permissions

### Using presigned URLs

Generate a presigned URL to test if a specific user can access an object:

```bash
AWS_ACCESS_KEY_ID=ci-user-key \
AWS_SECRET_ACCESS_KEY=ci-user-secret \
aws s3 presign s3://builds-bucket/v1.0/app.zip \
  --endpoint-url https://files.example.com \
  --expires-in 3600

# Try the URL in a browser or curl
curl -o /dev/null -w "%{http_code}" "https://files.example.com/builds-bucket/..."
# 200 = allowed, 403 = denied
```

### Using the AWS CLI

```bash
# Test LIST permission
AWS_ACCESS_KEY_ID=ci-user-key \
AWS_SECRET_ACCESS_KEY=ci-user-secret \
aws s3 ls s3://builds-bucket/v1.0/ \
  --endpoint-url https://files.example.com

# Test PUT permission
echo "test" | AWS_ACCESS_KEY_ID=ci-user-key \
AWS_SECRET_ACCESS_KEY=ci-user-secret \
aws s3 cp - s3://builds-bucket/v1.0/test.txt \
  --endpoint-url https://files.example.com
```

---

## Condition Reference

### Supported condition operators

| Operator | Description | Example |
|----------|-------------|---------|
| `StringEquals` | Exact string match | `s3:prefix` = `"docs/"` |
| `StringNotEquals` | Exact string non-match | `s3:prefix` != `"internal/"` |
| `StringLike` | Glob pattern match | `s3:prefix` LIKE `"user-*"` |
| `StringNotLike` | Glob pattern non-match | `s3:prefix` NOT LIKE `".*"` |
| `IpAddress` | CIDR range match | `aws:SourceIp` in `10.0.0.0/8` |
| `NotIpAddress` | CIDR range non-match | `aws:SourceIp` NOT in `203.0.113.0/24` |

### Supported condition keys

| Key | Type | Available on | Description |
|-----|------|-------------|-------------|
| `aws:SourceIp` | IP address | All requests | Client IP (from X-Forwarded-For or direct connection) |
| `s3:prefix` | String | LIST requests | The `prefix` query parameter |

### Condition JSON format

Conditions in the admin GUI map to this JSON structure:

```json
{
  "IpAddress": {
    "aws:SourceIp": "10.0.0.0/8"
  },
  "StringNotLike": {
    "s3:prefix": "internal/*"
  }
}
```

Multiple values for the same key are ORed:

```json
{
  "IpAddress": {
    "aws:SourceIp": ["10.0.0.0/8", "172.16.0.0/12"]
  }
}
```

---

## Common Patterns

### Pattern 1: Read-only public, write from office only

```
Rule 1: Allow read, list on * (no conditions)
Rule 2: Allow write on * + IpAddress aws:SourceIp 10.0.0.0/8
Rule 3: Deny delete on * (no conditions)
```

### Pattern 2: Per-team prefix isolation

```
Team A group:
  Allow * on data-bucket/team-a/*
  Deny list on data-bucket/* + StringNotLike s3:prefix "team-a/*"

Team B group:
  Allow * on data-bucket/team-b/*
  Deny list on data-bucket/* + StringNotLike s3:prefix "team-b/*"
```

### Pattern 3: CI with minimal permissions

```
Allow read, write, list on artifacts-bucket/builds/*
  + IpAddress aws:SourceIp "10.0.0.0/8"
Deny delete on * (prevent accidental deletion)
Deny write on artifacts-bucket/releases/* (releases are immutable)
```
