# wf-vendor-probe

[日本語](README_ja.md)

A read-only investigation CLI that digs DE's API response JSON out of the Warframe client's memory.

It was built to find out what arrives the moment a vendor is "updating its stock", but swapping the
needle points it at any JSON response.

---

## How it works

The Warframe client keeps the JSON it receives from DE's servers **in the heap as plain text even
after parsing it**. So no struct layouts, no pointer chains and no reverse engineering are needed —
a string search is enough. That is also why game updates do not break it: JSON key names stay put.

1. Find the process with `CreateToolhelp32Snapshot`
2. `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION)` — **read-only**
3. Walk the address space with `VirtualQueryEx`, read it with `ReadProcessMemory`
4. Search for the needle (`"ItemManifest"` and the like) with `memchr`
5. Walk **backwards from the hit, counting depth**, to find the unclosed `{` that encloses it
6. From there match braces forward, quotes taken into account, to settle the end, and verify with `serde_json`
7. Save only what passes verification into `out/`

Nothing is written, injected, hooked or patched.

---

## Build

```
cargo build --release
```

produces `target/release/wf-vendor-probe.exe`. Windows only.

---

## Use (walking a vendor investigation)

### Step 0 — check the connection

```
wf-vendor-probe regions
```

`attached to pid ...` means read access is there. If it does not appear, try running at the same
privilege level as the game (if the game is elevated, the probe has to be too).

Auto-detection prefers the game itself (`Warframe.x64.exe` / `Warframe.exe`), so another process
with a similar name is never attached to by mistake. `--pid` names one explicitly.

### Step 1 — self-test the pipeline

```
wf-vendor-probe extract --preset inventory
```

Recovering one full account inventory blob (a few MB) means every stage from search to
reconstruction works. If nothing comes out here, the problem is process access, not the needle.

### Step 2 — stake out the vendor (the real thing)

```
wf-vendor-probe watch
```

Leave this running, then talk to the vendor in game. On the pass right after "updating its stock"
finishes, only the newly appeared JSON lands in `out/` (identical content is dropped by hash).

A pass takes roughly one or two seconds, so the default three-second interval hardly ever misses
anything.

### Step 3 — when nothing comes out

The needle may be the wrong one, so look at the raw strings first.

```
wf-vendor-probe strings --filter vendor       > vendor-strings.txt
wf-vendor-probe strings --filter storeitem    > store-strings.txt
wf-vendor-probe extract --preset vendor-wide
wf-vendor-probe extract --preset api          # find the endpoint names being called
```

Once a promising key name turns up, it becomes the needle.

```
wf-vendor-probe watch --needle '"MyNewKey"' --needle "/Lotus/Types/Something/"
```

---

## Commands

| Command | Purpose |
|---|---|
| `watch` | Repeat the extract pass on an interval, saving **only content not seen before**. The workhorse |
| `extract` | One pass: reconstruct and save the JSON |
| `probe` | Report hits and the text around them without reconstructing JSON. Fast reconnaissance |
| `strings` | Dump every printable ASCII run. For when no needle is known yet |
| `regions` | Summarise the address space. Sanity check and troubleshooting |
| `presets` | List the built-in needle sets |

For the main options see `wf-vendor-probe --help`.

---

## Needle sets (presets)

| Name | Contents |
|---|---|
| `vendor` | The keys a vendor's stock always carries. The default; few false positives |
| `vendor-wide` | The above plus each offer's own fields. Fewer misses, more noise |
| `api` | Request URLs and endpoint names, to work out what is being called |
| `inventory` | The full account blob, as a self-test of the pipeline |

---

## Measured notes

Checked against a running client while this repository was written.

- A full pass (every region) takes **about 1.1 s**. `--fast` is rarely needed
- `--preset inventory` recovered a **2.43 MB** account blob
- `--preset vendor` produced:
  - **individual offers**
    `{"StoreItem": "/Lotus/StoreItems/...", "ItemPrices": [{"ItemType": "...", "ItemCount": 35}], "Bin": "BIN_0", "Expiry": {...}, "QuantityMultiplier": 1, "AllowMultipurchase": true, "Id": {"$oid": "..."}}`
  - **purchase history**
    `{"PurchaseHistory": [{"ItemId": "...", "NumPurchased": 1, "Expiry": {...}}], "VendorType": "/Lotus/Types/Game/VendorManifests/TheHex/Nova1999ConquestShopManifest"}`

### The observation that matters

With no vendor open, **the outer envelope `"ItemManifest"` is not resident**. What stays behind is
fragments of individual offers and the purchase history. Catching the envelope itself means having
`watch` running when the vendor is opened.

Even when only individual offers come out, **the address is a clue**. Offers of the same vendor are
allocated next to each other on the heap, so ordering by the address in the output file name groups
them per vendor. A purchase-history object carrying `VendorType` nearby says which vendor it is.

---

## About performance

Regions with `protect=0x404` (`PAGE_WRITECOMBINE`, GPU staging buffers) are excluded by default.
On this client they run past 4 GB, and being uncached they are extremely slow to read: 26 seconds
a pass before excluding them, one second after. JSON never lands there, so nothing is lost by
leaving them out. `--include-wc` takes them anyway.

---

## Caution

- Read-only as it is, reading a game process's memory sits in a grey area of the Warframe EULA.
  DE's position is that third-party tools are used at your own risk.
- **Never** reach for writing, injection or hooks. As long as this stays read-only it stands on the
  same ground as existing overlays such as Overwolf.
- The JSON this turns up carries account-specific IDs. Look through it before sharing any of it.

---

## API findings

Everything below was confirmed by actually making the requests (2026-08-21).

### The getVendorInfo endpoint

The assembled URL was sitting in the client's memory as it is.

```
GET https://api.warframe.com/api/getVendorInfo.php
      ?accountId=<24 hex digits>&nonce=<digits>&ct=STM&vendor=<full manifest path>
```

`vendor=` takes the path as it is (for example
`/Lotus/Types/Game/VendorManifests/Solaris/DebtTokenVendorManifest`). The URL for
`updateSession.php` was resident in the same place.

How to find them:

```
wf-vendor-probe strings --filter ".php" --include-exec
wf-vendor-probe strings --filter "vendor=" --include-exec
```

### Authentication is required, and cannot be walked past

| Request | Response |
|---|---|
| No credentials | `HTTP 500` (no body) |
| `ct=STM` added, still no credentials | `HTTP 500` (no body) |
| Well-formed but bogus accountId + nonce | `HTTP 409` `Log-in expired` |

That 500 and 409 are told apart is the important part: the server treats **a malformed request** and
**an invalid session** as different things. Authentication cannot be skipped; a live, valid session
is needed.

For a weekly batch job, treat 409 as the signal to log in again.