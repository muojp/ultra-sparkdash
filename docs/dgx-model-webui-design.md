# モデルデプロイWeb UI — 最小構成案

2026-09-20。設計のみ、未実装。既存sparkDashに1画面を追加し、CLIと同じ構成定義・
制御処理を使う。前提となる制御設計は [topology設計](dgx-model-topology-design.md)。
稼働モデルや既存UIはこの文書作成では変更しない。

## 利用者が選ぶもの

「モデル → 実行構成 → デプロイ」の3段階にする。単なる台数の選択にしない。
2台で1モデルを動かす構成と、2台に別々のサーバーを置く構成は明確に分ける。

| 画面の選択肢 | 意味 | 使用ノード | 接続先 |
|---|---|---|---|
| 1台で実行 | モデル全体を1台に配置 | dgx01（対応profileがあればdgx02も） | 1本 |
| 2台で分散実行 | 2台を使って1モデルを実行 | dgx01 + dgx02 | 通常1本 |
| 2台で並列受付 | 同じモデルの独立サーバーを各1台に配置 | dgx01 + dgx02 | 2本、round-robin |

2台必須のレシピには分散実行のみ表示し、「この構成は2台必要」と説明する。
1台対応レシピには定義されたsingle/replicated候補を表示する。
未検証・未準備の候補は理由とともに表示できるが、デプロイ可能として扱わない。
モデルのパラメータ数や空きRAMから対応台数を推測しない。

初期カタログの分類（対応の記述であり、現在のready判定ではない）:

| モデル・レシピ | 提供する構成 |
|---|---|
| DeepSeek V4 Flash / V4.1 Flash の既存レシピ | 2台分散 |
| GLM5.3 Flash の既存3レシピ | 各レシピの2台分散を別profileとして保持 |
| Qwen3.8 27B SGLang | 1台、2台並列受付。既存2台profileを維持し単体profileを追加 |
| Qwen3.8 Flash Next 単体レシピ | 1台。2台並列受付は後日provision・測定してから追加 |
| Qwen3.8 Flash Next dualレシピ | 2台分散。単体レシピとは重み・レシピが異なる候補 |

Qwen Flash Nextの単体とdualは同じモデル見出しの下にまとめられるが、単体の重みと
2台版の重みを同一扱いしない。選択カードに重み形式・レシピ名・対応context・
その構成の測定結果へのリンクを表示する。技術的な引数やrevisionは詳細欄へ置く。

## 最小の1画面

sparkDashに「モデル」ページを追加する。ノード別の監視ページにデプロイボタンを
散らさず、fleet全体の操作として入口を1つにする。

1. **稼働状況**: 現在のモデル、構成、dgx01/dgx02それぞれの使用状態、APIのready。
   unused / stopped / unreachableを区別し、最後の取得時刻を表示。
2. **モデルと構成選択**: 対応候補のみ選択できる。単体で配置先候補が複数あるときだけ
   ノード選択を表示。選択の初期値は現在構成、初回は明示された既定profile。
3. **切り替え内容**: 選択するとplanを取得。「現在の○○を停止 → dgx01で△△を起動、
   dgx02は未使用」、対象endpoint、実測に基づく起動目安、未準備の理由を表示。
4. **デプロイ**: planが有効なら1回のクリックで実行。表示済みplanに対する操作を意思確認とし、
   同じ内容の確認ダイアログを重ねない。
5. **進行状況**: 事前確認 → 停止 → 準備 → 起動 → API確認 → 完了。
   経過時間と現在の工程を表示し、見積もりに基づく架空の進捗%は出さない。
   再読込・ブラウザを閉じても処理は継続し、再接続で同じjobを表示。
6. **結果**: 成功時はノード別readyと接続先、失敗時は失敗工程・cleanup結果・ログ・
   明示的な再試行。自動restart/rollbackはしない。

停止は稼働構成の操作として用意する。restartと詳細ログは補助操作に置く。
起動中の任意cancel、設定ファイル編集、任意shell、重量級モデルのWebからのdownloadは
初版に含めない。未準備profileには不足項目を表示する。
ベンチは初版では既存測定の表示・リンクまでとし、標準bench起動ボタンは後段に分ける。

## 構成定義を唯一の根拠にする

CLIとUIでモデル一覧・対応台数を別々に持たない。head上のdgx-model registryから
次の情報を返す。既存TOMLコメントの「NOT PROVISIONED」は古いものもあるため、
画面の準備状態には使用しない。

- family_id / 表示名: モデルの見出し。
- profile_id: 実行構成の固定ID。履歴と測定結果はこのIDに紐づく。
- topology / nodes / endpoints: 分散と並列受付を区別する構造化データ。
- recipe / checkpoint / runtime設定: revisionと必要ファイルの検証情報。
- support: 定義・検証済みか。provision: 各ノードに必要物が揃っているか。
- observed: 現在の実機状態。selectedや前回成功履歴とは別。
- lane: 単体ならendpoint1本、replicatedなら2本の配送先と明示された合計sessions。
- measurements: 同じprofileの結果への参照。1台の測定値を2倍して2台値にしない。

バックエンドもprofileとnodeの組合せを検証する。ブラウザで無効化しただけでは足りない。
既存profileは明示的に分類し、互換adapter経由で段階移行する。

## 処理の配置

```text
ブラウザ: sparkDash /models
   ↓ 同一originのHTTP
sparkDash server: fleet API adapter
   ↓ 既存SSH経路、固定コマンド・構造化JSON
head: dgx-model controller + persistent operation worker
   ↓ profileのレシピのみ実行
 dgx01 / dgx02
```

現状sparkDashはReact/TypeScriptとExpressで、SSH経由のホスト操作を持っている。
ここにfleet API adapterを追加する。dgx-modelの切り替え判断をJavaScriptに再実装しない。
headにはoperationを永続化し、ブラウザ/HTTP/SSHセッションから独立して進めるworkerを
置く。Web専用の別制御経路を作らず、CLIのswitchも同じplan/job/lockを使う。
CLIは通常job完了まで待ち、UIはjob IDを受け取ってpollする。

controllerの再起動はモデルの自動再起動とは別。中断operationを実機から照合し、
未確認のstartを自動再実行しない。controller停止でもvLLMの生存を妨げない。

## 最小API契約（案）

| API | 役割 |
|---|---|
| GET `/api/fleet/catalog` | モデル、対応profile、準備状況、測定への参照 |
| GET `/api/fleet/status` | 実機状態、選択構成、操作中job |
| POST `/api/fleet/plans` | profile IDと操作から停止・起動範囲、不足項目を計算 |
| POST `/api/fleet/operations` | plan ID・idempotency keyを受け付け、202 + job ID |
| GET `/api/fleet/operations/:id` | 工程、結果、ログ差分cursor |

planは設定revisionと観測状態のfingerprint、有効期限を持つ。実行時はheadのlock下で
再検査する。変更されていたら409で再planを返し、未確認の追加ノードを停止しない。
二重クリック・通信切断後の再送は同じjobに紐づけ、操作中の競合要求は409。
SSH不能・API不能を「何も動いていない」に変換しない。

外部入力としてSSH先、path、shell文字列は受け取らず、登録IDと列挙値だけを許す。
既存LAN用dashboardの公開範囲は広げない。mutation APIはsame-origin検証とCSRF対策を
行い、ログやAPI応答に認証情報を出さない。既存認証があるとは仮定しない。

## laneとの連動と最初の運用範囲

起動するだけでなく、replicated選択時に利用側が2endpointへround-robinする必要がある。
benchだけが2台を使う状態を「2台の運用が完了」と表示しない。

UIは `engine ready` と `利用側の接続設定` を分けて表示する。既存laneの
pause/drain → endpoint・model・sessions更新 → resume は外部側のadapter契約として
接続し、利用側まで切り替えるモードではdrain失敗時にエンジンを停止しない。
未連携ならengineのデプロイ完了までを表示し、laneまで切り替えたとは表示しない。
既存laneは別workspaceにあるため、adapterの入出力・接続権限は実装時に確認する。

初版はfleet全体で1profileだけ選ぶ。single選択時の余ったノードは未使用。
別モデルの共存は占有管理・複数active・利用側routingを要するため後段にする。
この制限を画面にも明記する。

## 実装の区切り

1. **制御基盤**: explicit topology/profile、準備検査、対象限定操作、全endpointのready、
   plan・永続job・ロック。Qwen27Bにsingle/replicatedを用意し、既存IDは2台として維持。
2. **最小UI**: 上記1画面、catalog/status/plan/deploy/progress/stop。
   2台必須のsingle選択禁止、未準備理由、再接続、失敗からの明示再試行まで。
3. **運用連動**: lane adapterの接続とround-robin確認。UIでの標準bench実行、
   新Qwenの2台並列受付、複数モデル共存はそれぞれ独立した追加。

受け入れ確認はQwen27Bの1台↔2台並列、2台分散モデル↔新Qwen単体、
未準備profile拒否、片側到達不能、二重実行、ブラウザ再読込、起動失敗cleanup。
UIの文言・状態が実機と一致し、unusedノードに不要な操作をしないことを検証する。
