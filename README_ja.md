# wf-vendor-probe

[English](README.md)

Warframe クライアントのメモリから、DE の API レスポンス JSON を読み取り専用で探し出す調査用 CLI。

ベンダーの「売り物をアップデートしています」の瞬間に何が飛んでくるのかを特定するために作られたが、
針（needle）を差し替えれば任意の JSON レスポンスに使える。

---

## 動作原理

Warframe クライアントは DE のサーバーから受け取った JSON を、**パース後もヒープ上に生テキストのまま保持する**。
だから構造体レイアウトもポインタチェーンもリバースエンジニアリングも不要で、文字列検索だけで済む。
ゲームアップデートで壊れないのもこの性質のおかげ（JSON のキー名は変わらない）。

1. `CreateToolhelp32Snapshot` でプロセスを特定
2. `OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_INFORMATION)` — **読み取り専用**
3. `VirtualQueryEx` でアドレス空間を走査し、`ReadProcessMemory` で読み出す
4. 針（`"ItemManifest"` など）を `memchr` で検索
5. ヒット位置から**深さを数えながら逆走**し、それを囲む未閉じの `{` を特定
6. そこから引用符を考慮した前方ブレースマッチで終端を確定し、`serde_json` で検証
7. 検証を通ったものだけを `out/` に保存

書き込み・インジェクション・フック・パッチは一切行わない。

---

## ビルド

```
cargo build --release
```

`target/release/wf-vendor-probe.exe` ができる。Windows 専用。

---

## 使い方（ベンダー調査の手順）

### 手順 0 — 接続確認

```
wf-vendor-probe regions
```

`attached to pid ...` が出れば読み取り権限は取れている。出ない場合は
「同じ権限レベルで起動する（ゲームが管理者なら probe も管理者）」を試す。

自動検出はゲーム本体（`Warframe.x64.exe` / `Warframe.exe`）を優先するので、
名前の似た別のプロセスが動いていても誤って接続しない。`--pid` で明示指定もできる。

### 手順 1 — パイプラインの自己テスト

```
wf-vendor-probe extract --preset inventory
```

アカウントのフルインベントリ blob（数 MB）が 1 つ取れれば、
検索から復元までの全段が正常。ここで何も取れないなら、
問題は針ではなくプロセスアクセス側にある。

### 手順 2 — ベンダーを張る（本命）

```
wf-vendor-probe watch
```

これを起動したまま、ゲーム内で対象のベンダーに話しかける。
「売り物をアップデートしています」が終わった直後のパスで、
新しく現れた JSON だけが `out/` に落ちる（内容ハッシュで重複排除している）。

1 パスは概ね 1〜2 秒。デフォルトの 3 秒間隔なら取りこぼしはまず起きない。

### 手順 3 — 何も取れなかった場合

針が違う可能性があるので、まず生の文字列から探す。

```
wf-vendor-probe strings --filter vendor       > vendor-strings.txt
wf-vendor-probe strings --filter storeitem    > store-strings.txt
wf-vendor-probe extract --preset vendor-wide
wf-vendor-probe extract --preset api          # 叩いているエンドポイント名を探す
```

有望なキー名が見つかったら、そのまま針にできる。

```
wf-vendor-probe watch --needle '"MyNewKey"' --needle "/Lotus/Types/Something/"
```

---

## コマンド

| コマンド | 用途 |
|---|---|
| `watch` | 一定間隔で extract を繰り返し、**未見の内容だけ**保存する。調査の主力 |
| `extract` | 1 パスだけ実行して JSON を復元・保存 |
| `probe` | JSON 復元せず、ヒット位置と前後のテキストだけ表示。高速な偵察 |
| `strings` | 印字可能 ASCII を全部ダンプ。針が分からない段階の探索用 |
| `regions` | アドレス空間の概況。接続確認とトラブルシュート |
| `presets` | 組み込みの針セット一覧 |

主なオプションは `wf-vendor-probe --help` を参照。

---

## 針セット（プリセット）

| 名前 | 内容 |
|---|---|
| `vendor` | ベンダー在庫の確実なキー。既定値。誤検出が少ない |
| `vendor-wide` | 上記＋各オファーのフィールド。取りこぼしを減らすがノイズも増える |
| `api` | リクエスト URL とエンドポイント名。何を叩いているかの特定用 |
| `inventory` | フルアカウント blob。パイプラインの自己テスト用 |

---

## 実測メモ

このリポジトリ作成時に、実際に動作中のクライアントに対して確認した結果。

- フルパス（全領域）で **約 1.1 秒**。`--fast` はほぼ不要
- `--preset inventory` で **2.43 MB** のアカウント blob を復元できた
- `--preset vendor` で以下が取れた:
  - **個別オファー**
    `{"StoreItem": "/Lotus/StoreItems/...", "ItemPrices": [{"ItemType": "...", "ItemCount": 35}], "Bin": "BIN_0", "Expiry": {...}, "QuantityMultiplier": 1, "AllowMultipurchase": true, "Id": {"$oid": "..."}}`
  - **購入履歴**
    `{"PurchaseHistory": [{"ItemId": "...", "NumPurchased": 1, "Expiry": {...}}], "VendorType": "/Lotus/Types/Game/VendorManifests/TheHex/Nova1999ConquestShopManifest"}`

### 重要な観察

ベンダーを開いていない状態では、`"ItemManifest"` という**外側のエンベロープは常駐していない**。
残っているのは個別オファーの断片と購入履歴だけ。
エンベロープごと捕まえたいなら、`watch` を回した状態でベンダーを開く必要がある。

なお個別オファーしか取れない場合でも、**アドレスが手がかりになる**。
同じベンダーのオファーはヒープ上で隣接して確保されるため、
出力ファイル名に含まれるアドレス順に並べれば、ベンダー単位でグルーピングできる。
`VendorType` を持つ購入履歴オブジェクトが近くにあれば、どのベンダーかも特定できる。

---

## 性能について

`protect=0x404`（`PAGE_WRITECOMBINE`、GPU ステージングバッファ）の領域は既定で除外している。
このクライアントでは 4 GB 以上あり、アンキャッシュドなので読み出しが極端に遅い。
除外前は 1 パス 26 秒、除外後は 1 秒。JSON がそこに載ることはないので、除外して失うものはない。
どうしても含めたい場合は `--include-wc`。

---

## 注意

- 読み取り専用とはいえ、ゲームプロセスのメモリを読む行為は Warframe EULA のグレーゾーンにある。
  DE は「サードパーティツールは自己責任」という立場。
- 書き込み・インジェクション・フックには**決して**手を出さないこと。
  読み取り専用である限り、Overwolf 等の既存オーバーレイと同じ土俵に留まれる。
- 調査で得た JSON にはアカウント固有の ID が含まれる。共有する前に中身を確認すること。

---

## API 調査の実測結果

以下はすべて実際にリクエストして確認した結果（2026-08-21）。

### getVendorInfo エンドポイント

クライアントのメモリに、組み立て済みの URL がそのまま残っていた。

```
GET https://api.warframe.com/api/getVendorInfo.php
      ?accountId=<24桁hex>&nonce=<数字>&ct=STM&vendor=<マニフェストのフルパス>
```

`vendor=` にはパスをそのまま渡す（例: `/Lotus/Types/Game/VendorManifests/Solaris/DebtTokenVendorManifest`）。
同じ場所に `updateSession.php` の URL も常駐していた。

見つけ方:

```
wf-vendor-probe strings --filter ".php" --include-exec
wf-vendor-probe strings --filter "vendor=" --include-exec
```

### 認証は必須（回避不可）

| リクエスト | 応答 |
|---|---|
| 認証情報なし | `HTTP 500`（本文なし） |
| `ct=STM` のみ追加、認証なし | `HTTP 500`（本文なし） |
| 形式は正しいがデタラメな accountId + nonce | `HTTP 409` `Log-in expired` |

500 と 409 が区別されている点が重要で、**リクエスト形式の不備**と**セッション無効**を
サーバーが別扱いしている。つまり認証は素通りできず、有効な生きたセッションが必要。

週次バッチを組むなら、409 を「再ログインの合図」として扱えばよい。