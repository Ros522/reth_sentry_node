# reth-sentry-node

Ethereum P2P ネットワークに接続して mempool トランザクションを収集し、バックエンドノードに配信する軽量 sentry ノード。

[reth](https://github.com/paradigmxyz/reth) の P2P スタック (devp2p / discv4 / RLPx / eth プロトコル) を使用。**ブロック同期は一切行わない。**

## Architecture

```
Ethereum P2P Network
        │
   NetworkManager (reth-network v1.11.3)
        │
        ├── NewBlock → BlockCache (LRU 256)
        │                  │
        │           GetBlockHeaders/Bodies → Peer
        │
        └── Transaction Gossip
                  │
           StatelessValidator
                  │ (valid only)
              TxDedup (LRU 100K)
                  │ (unique only)
            ┌─────┴─────┐
        WsBroadcaster   HTTP RPC
         (port 8546)    (eth_sendRawTransaction)
            │               │
        Backends         Backends
```

## Features

- **ブロック同期なし** — `NoopProvider` でブロック/ステート同期を完全無効化
- **ステートレス tx バリデーション** — chain state 不要な検証のみ実施:
  - Chain ID チェック
  - Gas limit 範囲 (21,000 ~ 30M)
  - EIP-1559 fee ordering (`maxPriorityFee ≤ maxFee`)
  - Gas price 非ゼロ
  - Value + gas cost オーバーフロー
  - 署名検証 (Recovered<T> で暗黙保証)
  - Nonce != u64::MAX
  - コントラクト作成 tx 除外
- **WebSocket 配信** — バックエンドが pending tx をリアルタイム受信
- **HTTP RPC 転送** — `eth_sendRawTransaction` で既存ノードにも転送
- **LRU 重複排除** — 複数ピアからの同一 tx をスキップ、統計ログ付き
- **NewBlock キャッシュ** — ピアの GetBlockHeaders/Bodies にキャッシュから応答し peer reputation 維持
- **ノードキー永続化** — 再起動後も peer ID を維持
- **Graceful shutdown** — Ctrl+C で全タスクをクリーンに停止

## Quick Start

### Build

```bash
cargo build --release
```

### Run

```bash
# WebSocket 配信のみ (バックエンドが ws://localhost:8546 に接続して受信)
./target/release/reth-sentry-node

# HTTP 転送も併用
./target/release/reth-sentry-node --backends http://node1:8545,http://node2:8545

# 設定ファイルで起動
./target/release/reth-sentry-node --config sentry.toml
```

### CLI Options

| オプション | デフォルト | 説明 |
|---|---|---|
| `--port` | `30303` | P2P listen port |
| `--max-peers` | `50` | 最大ピア数 |
| `--chain-id` | `1` | Chain ID (1 = mainnet) |
| `--backends` | (なし) | HTTP RPC エンドポイント (カンマ区切り) |
| `--ws-port` | `8546` | WebSocket server port |
| `--no-ws` | `false` | WebSocket server を無効化 |
| `--data-dir` | `data` | ノードキー等の保存先 |
| `--config` | (なし) | TOML 設定ファイルパス |

## WebSocket API

バックエンドは WebSocket で接続して pending tx をストリーミング受信できる。

### 接続

```bash
wscat -c ws://localhost:8546
```

### Subscribe

```json
{"subscribe": "pending_transactions", "mode": "raw"}
```

| mode | 説明 |
|---|---|
| `raw` | hash + raw tx bytes (hex) |
| `hash` | hash のみ (軽量) |

### 受信メッセージ

**raw モード:**
```json
{"hash": "0xabc...", "raw_tx": "0xf86c..."}
```

**hash モード:**
```json
{"hash": "0xabc..."}
```

## Configuration (TOML)

```toml
[network]
chain_id = 1
max_peers = 50
p2p_port = 30303
discovery_port = 30303

[backend]
endpoints = ["http://localhost:8545"]
max_concurrent = 64
buffer_size = 4096

[backend.dedup]
cache_size = 100000
log_interval = 1000

[websocket]
enabled = true
addr = "0.0.0.0"
port = 8546
capacity = 4096
```

## Project Structure

```
src/
├── main.rs          # エントリポイント、CLI、設定読み込み
├── network.rs       # P2P ネットワークセットアップ
├── validator.rs     # ステートレス tx バリデーション
├── forwarder.rs     # HTTP RPC 転送 + WS ブロードキャスト
├── ws_server.rs     # WebSocket サーバー
├── dedup.rs         # LRU ベース tx 重複排除
├── block_cache.rs   # NewBlock LRU キャッシュ
├── block_import.rs  # カスタム BlockImport (キャッシュ書き込み)
├── eth_proxy.rs     # ETH プロトコルリクエストハンドラ
├── node_key.rs      # ノードキー永続化
└── config.rs        # TOML 設定
```

## Log Examples

```
INFO  === Reth Sentry Node ===
INFO  chain_id: 1
INFO  p2p_port: 30303
INFO  loaded existing node key path="data/node.key"
INFO  WebSocket server started, waiting for backend connections addr=0.0.0.0:8546
INFO  sentry node started, listening for peers peer_id=0x04abc...
INFO  new peer connected peer_id=0x04def... client_version="Geth/v1.14.0"
DEBUG new pending transaction detected tx_hash=0x123...
INFO  dedup stats total=1000 unique=620 duplicates=380 dup_rate="38.0%"
INFO  peer count update num_peers=25
INFO  received Ctrl+C, initiating graceful shutdown...
INFO  network stopped
INFO  sentry node shut down cleanly
```

## License

MIT
