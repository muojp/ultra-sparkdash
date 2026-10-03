# dgx-model の単体構成制御案

2026-09-20。設計案であり、以下の新しい設定・CLI は未実装。
今回の作業は設計文書とその案内のみ。稼働中のモデルは変更しない。

Web UIからの構成選択・デプロイは [最小Web UI設計](dgx-model-webui-design.md) を参照。

## 現状と不足

`qwen3.8-flash-next-single` は `[containers] head=[...] / worker=[]` と head の
start/stop により起動でき、実際に単体測定も済んでいる。ただし「単体を正式に管理する」
機構ではなく、既存の fleet 一括切り替えに載せた状態。

| 箇所 | 現実装 | 必要な変更 |
|---|---|---|
| `switch` | 観測できた全deploymentを停止 | 当面この排他契約を維持し、計画に停止対象を明示 |
| `drop_caches` | head と dgx02 を固定で操作 | 新deploymentの使用ノードのみ。必要性・実行結果を履歴に記録 |
| `status` | headコンテナと代表APIで判定、`active` は単一文字列 | 必須ノード・全endpointを検証、到達不能と停止を区別 |
| `wait_served` | 代表APIのモデル名のみ確認 | 必須コンテナと全endpointのreadyを確認 |
| `recipe_present` | head のディレクトリ・起動スクリプトのみ | 使用ノードすべてのrecipe、設定、重み、imageを停止前に確認 |
| 起動コマンド | head実行、worker操作はargv内にsshを手書き | 実行ノードを構造化。分散recipeの順序・一括操作は維持 |
| ベンチ | `api_urls` のpoolを使うが事前確認は代表APIのみ | 全endpointの所属・ready確認、対象ノードと構成を結果に保存 |
| 停止・障害復旧 | 公開CLIにstopがなく、switchは対象がactiveなら即終了 | 明示stopとrestart、部分起動の処理を定義 |

Docker問い合わせの失敗が空リストになるため、現在は「不明」を「停止」と誤認し得る。
また `active` 判定だけでswitchを省略すると、単体×2の片方故障を見逃す。
前回の「切り替えられる」はhead単体の現行レシピが動く範囲の説明であり、これらを含む
一般的な単体制御が完成したという意味ではない。

## 推奨する運用契約

最初の実装では **fleetに同時に選択するdeploymentは1つ** とする。
deploymentはレシピ・推論設定・使用ノード・endpointを固定した実行構成。
`head/worker` は推論上の役割と混同するため、ノード識別子には `dgx01/dgx02` を使う。
管理コマンドの実行拠点は引き続きdgx01。dgx02単体でも管理の独立性・HAは保証しない。

| 構成 | mode | nodes | endpoint数 | 切り替え・障害の扱い |
|---|---|---|---:|---|
| 1台のみ | `single` | `[dgx01]` または `[dgx02]` | 1 | その1台の起動・停止・ready |
| 2台で1モデル | `distributed` | `[dgx01,dgx02]` | 通常1 | 一体で起動・停止。片側故障で全体failed |
| 同一モデルを各1台 | `replicated` | `[dgx01,dgx02]` | 2 | 両方readyで成功。片側故障はdegraded |

新Qwenはまず `single / dgx01` のprofile。Qwen27Bは **1台構成と2台ラウンドロビン構成を
選択可能にする**。同一レシピでもsingleとreplicatedには別deployment IDを与え、
測定結果・lane設定を分離する。利用者にはfamily名と構成選択を提供し、検証済みprofileへ
解決する。レシピのTPや通信設定をCLIが推測したり、その場で任意の構成を生成したりしない。

構成選択のCLI案（未実装）:

```sh
# Qwen27Bを1台で。node省略時はdgx01。
dgx-model switch qwen3.8-27b-sglang --topology single --node dgx01
# 既存と同じ、各ノード1サーバー・ラウンドロビン。
dgx-model switch qwen3.8-27b-sglang --topology replicated
# 構成候補・provision済みか・endpointを表示。
dgx-model list --variants
```

`--topology` を省略した既存のQwen27Bコマンドは、従来どおり2台構成を選ぶ。
`--node` はsingleのときだけ受け付ける。dgx02単体は事前に用意したprofileがある場合のみ許可。
2台を使うことだけではdistributedとreplicatedを区別できないため、`--nodes 2`は採用しない。
新Qwenにreplicatedを指定しても、用意・検証していない段階では停止前に拒否する。

内部profileの例:

| family | 選択 | 解決先deployment ID | lane/benchの接続先 |
|---|---|---|---|
| `qwen3.8-27b-sglang` | single, dgx01 | `qwen3.8-27b-sglang-single-dgx01` | dgx01のみ |
| `qwen3.8-27b-sglang` | replicated | `qwen3.8-27b-sglang`（既存ID維持） | dgx01/dgx02のround-robin |
| `qwen3.8-flash-next-single` | single, dgx01 | `qwen3.8-flash-next-single`（既存ID維持） | dgx01のみ |

IDはレシピの違いも保持する。family→topology/node→profileの対応を明示的なregistryに持ち、
一覧・dry-run・historyに解決後IDを必ず出す。構成選択はswitch時に行い、benchは現在の
解決後IDを引き継ぐ。稼働構成と異なるprofileで測定する要求は拒否する。
既存の2台Qwen27B測定は旧IDのまま保持し、新しい単体結果を上書きしない。


## 設定の形（案）

共有 `nodes.toml` にSSH経路と、クライアントの位置ごとのAPI到達先を定義する。
Mac→dgx02の制御はdgx01経由。Macから到達できないAPIにはbenchクライアントをdgx01へ置く。

既存deploymentに、次の情報を追加する。ここでは新Qwenの例を示す。

```toml
schema_version = 2
name = "qwen3.8-flash-next-single"

[topology]
mode = "single"
nodes = ["dgx01"]

[[endpoints]]
node = "dgx01"
url = "http://192.168.0.100:8888"
served_model = "qwen3.8-flash-next-single"

[placement.dgx01]
recipe_dir = "/home/muo/Qwen3.8-Flash-Next-Single-DGX-Spark"
containers = ["vllm-fn-single"]

[[lifecycle.start]]
node = "dgx01"
argv = ["env", "MEMWATCH_ENABLED=0", "./start.sh"]

[[lifecycle.stop]]
node = "dgx01"
argv = ["./stop.sh"]

[lifecycle]
auto_restart = false
# 実行する場合のみ明示。キャッシュ操作は当該ノードの起動前に限る。
prepare_cache = "recipe"
```

設定は実装時に構成の意味までスキーマ検証する。
各stepは指定ノードのrecipe_dirで実行する。分散recipeはheadの既存クラスタ起動scriptを
1stepとして使えるようにし、CLI側でworker起動を二重実行しない。
Composeならplacementごとにproject selectorを使い、名前指定と排他的に検証する。
レシピrevision・image digest・重みrevisionと起動envはplan/historyに記録する。
認証情報は履歴に出力しない。

## CLI と状態

既存の `switch <deployment>` は引き続きfleetの排他切り替え。
以下の追加を提案する（未実装）。

```sh
dgx-model switch qwen3.8-flash-next-single --dry-run
dgx-model switch qwen3.8-flash-next-single
dgx-model stop qwen3.8-flash-next-single
dgx-model restart qwen3.8-flash-next-single
dgx-model status --json
dgx-model bench qwen3.8-flash-next-single -- --scenario all -c 1,2,4,8 --no-thinking
```

stopは指定deployment全体を止める。distributedの片側stopは公開しない。
restartも同じ設定・範囲で明示的に実行し、自動再起動はしない。
停止済みならstopは成功、全必須要素readyなら同一対象へのswitchは何もしない。
部分起動なら「already active」にせず、現状と明示restartの案内を返す。

状態は `stopped / starting / ready / degraded / failed / unknown`。
`selected` は希望したdeployment、実際の`state`は観測結果とする。
ノード別にコンテナ・API・モデルID・エラーを返し、未使用ノードは`unused`。
未使用ノードの到達不能だけで単体readyを失敗させない。ただし切り替え時、そのノードに
旧deploymentが残っている可能性があれば、排他性が確認できないので停止前に拒否する。

既存の`active`は互換出力としてreadyなdeploymentのみ返す。
`last_switch`は履歴であり現状の証拠にはしない。laneは現在の定義から返す。
replicatedの片側故障を黙って縮小運用しない。通常のpoolベンチは拒否し、単体測定なら
別profileへ切り替える。自動failoverやリバースプロキシはこの実装の範囲外。

## 切り替え手順

1. dgx01でfleet操作ロックを取得し、設定を検証する。
2. targetの全使用ノードのSSH、recipe、必要ファイル・重みrevision、imageを事前確認。
   旧deploymentと対象外プロセスの観測も行う。未知のGPUプロセスは自動killしない。
3. dry-runを含め、旧構成→新構成、停止対象、起動対象、API、準備操作をplanとして表示。
4. 利用側laneのdrainは既存の外部オーケストレーションで行うという境界を維持。
5. 旧deploymentのレシピstopを実行し、必須コンテナ終了とport解放を確認。
6. 新構成の使用ノードだけを準備して起動。未使用ノードにキャッシュ操作をしない。
7. 必須コンテナ、全endpoint、モデルIDを確認し、readyになってから状態を確定する。
8. 失敗時は今回起動した範囲だけをレシピstopでcleanup。cleanup失敗も履歴に残す。
   自動再起動・旧モデルへの自動rollbackはしない。状態と次の操作を提示する。

ロックはstop/restartも共有。起動タイムアウトはログ行が来ない間も効くようにする
（現行run_stepsのstdout逐次読みでは、無出力の子プロセスを確実に止められない）。
切断・プロセス終了で状態ファイルだけが進まないよう、操作中状態を原子的に記録し、
次回操作は実機の観測から整合させる。

## ベンチ・lane・telemetry

- ベンチ開始前に対象の全endpointを確認。実行中の同一対象へのswitch/stopと競合させない
  ため、ベンチも操作ロックと協調する。中断時のremoteジョブ残留も確認する。
- JSONにdeployment、topology、使用ノード、全endpoint、client位置、recipe/image/model
  revisionを保存。単体とpoolの集計を混ぜない。古い結果の構成を勝手に推定して書き換えない。
- 出力tokens / batch wallと入力tokens / batch wallを別指標として表示。
  memory floorは使用ノードを主表示し、未使用ノードは参考値。
- lane.sessionsはprofileの測定値で設定。replicated化しただけで自動2倍にしない。
  node別APIへの配送はlane側の契約が必要で、benchのround-robinだけでは運用poolは成立しない。
- 自動再起動なし、supervisor/timerなし、telemetry opt-out、offline設定はレシピ側で設定。
  dgx-modelは起動後の監査結果を保存する。ローカルmetricsは継続。

## 実装順と受け入れ条件

1. **単体を正式対応**: topology、配置、全対象ready判定、使用ノード限定の準備、
   stop/restart、操作ロック・履歴、ベンチ構成記録。既存2台構成は互換adapterで維持。
   topologyを空コンテナ配列から恒久的に推測せず、全8定義に明示的な分類を与える。
2. **Qwen27Bの構成選択**: family/profile解決とsingle/replicated選択を実装。
   既存replicatedの2endpoint判定、片側故障、lane側endpoint契約を整理。
   新Qwenの2台poolはここで別途判断し、単体結果から性能を外挿しない。
3. **必要になったら複数deployment共存**: `active`を複数instanceにし、ノード単位の占有・
   conflict graph・部分切り替え・lane routingを追加。段階1で空いたdgx02へ別モデルを
   同時起動する機能は提供しない。ポート8888の排他はホストごとでありfleet共通ではないが、
   当面fleet排他を維持するのは操作契約としての選択。

必須テスト: 単体→単体でdgx02へ操作しない／2台→単体では旧2台を停止し新headだけ起動／
単体→2台の事前検査失敗で現行を止めない／replicated片側故障をreadyにしない／
SSH不能をstoppedにしない／同一モデル名の別recipeを誤認しない／起動失敗cleanup／
無出力timeout／同時switchとベンチ競合／未使用ノードを単体ベンチに混ぜない。
追加テスト: Qwen27Bの構成省略は既存2台profile／single指定で接続先1本・停止起動範囲一致／
未定義variantの拒否は現行停止より前／singleとreplicatedの結果ID分離／laneの配送先も
構成と一致。実機受け入れはdry-run、単体stop/start、Qwen27Bの1台↔2台の往復と
round-robinの両endpoint利用を記録し、その時点でmake checkを通す。
