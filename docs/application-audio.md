# アプリケーションの音をパッチする（Chrome など）

PC 上のアプリが鳴らしている音を SoundNet の入力として扱う手順。
**イベントなど、無音が長く続いても止まってはいけない用途を前提に書く。**

---

## なぜそのままでは見えないか

`devices.rs` は `alsa::Card::iter` で実在のサウンドカードだけを列挙し、
`default:` / `dmix:` / `dsnoop:` などの ALSA プラグイン別名は意図的に捨てて
いる。Chrome の出力先は PipeWire であって ALSA のキャプチャデバイスでは
ないので、ポート一覧に現れない。

そこで `snd-aloop` を挟む。再生側に書いた音がキャプチャ側から読める
カーネルモジュールで、**両側とも本物の `hw:` カードとして見える**ため、
SoundNet は何も変更せずに扱える。

```
Chrome → PipeWire → hw:Loopback,0,0 (再生) → カーネル
                                            → hw:Loopback,1,0 (録音) → SoundNet
```

---

## 1. snd-aloop をロードする

```bash
sudo modprobe snd-aloop
echo snd-aloop | sudo tee /etc/modules-load.d/snd-aloop.conf
```

カード番号を固定したい場合は `/etc/modprobe.d/snd-aloop.conf` に
`options snd-aloop index=10` などを書く。SoundNet はデバイスをカード ID
（`hw:CARD=Loopback`）で参照するので番号がずれても壊れないが、他のツールと
併用するなら固定しておくと混乱が少ない。

---

## 2. 【重要】PipeWire にサスペンドさせない

**ここがイベント用途で最も大事な設定。**

PipeWire は既定で、しばらく何も鳴っていないノードをサスペンドして ALSA
デバイスを閉じる。閉じられると Loopback の再生側への書き込みが止まり、
録音側は何も返さなくなる。SoundNet からは「デバイスが応答しない」状態に
見え、ルートの health が `stalled` になる。**故障ではないが、イベント中に
これが起きると復帰まで音が出ない。**

WirePlumber 0.5 系（Debian trixie / Ubuntu 24.04）では、
`~/.config/wireplumber/wireplumber.conf.d/51-soundnet-loopback.conf` に:

```
monitor.alsa.rules = [
  {
    matches = [
      { device.name = "~alsa_card.platform-snd_aloop.*" }
    ]
    actions = {
      update-props = {
        session.suspend-timeout-seconds = 0
        api.alsa.period-size = 256
      }
    }
  }
  {
    matches = [
      { node.name = "~alsa_output.platform-snd_aloop.*" }
    ]
    actions = {
      update-props = {
        session.suspend-timeout-seconds = 0
        node.pause-on-idle = false
      }
    }
  }
]
```

`session.suspend-timeout-seconds = 0` が「サスペンドしない」の意味。
反映するには:

```bash
systemctl --user restart wireplumber
```

適用されたかどうかは、何も鳴らさずに数分放置してから
`wpctl status` でそのシンクが `suspended` になっていないことと、
SoundNet の health が `ok` のままであることで確認する。

> ノード名・デバイス名は環境によって違う。`wpctl status` と
> `pw-cli info <id>` で実際の `node.name` / `device.name` を確認してから
> 書くこと。上のパターンはよくある形だが、一致しなければ何も起きない
> （設定ミスは無言で効かないので、必ず放置テストで確かめる）。

---

## 3. Chrome の出力先を Loopback にする

```bash
pavucontrol   # 「再生」タブで Chrome の行のデバイスを Loopback に
```

Chrome は再生を始めて初めて「再生」タブに現れる。何か鳴らしながら設定する。

---

## 4. SoundNet 側

UI で **Rescan devices** を押すと `Loopback` が入力ポートに現れる。

**`plughw:CARD=Loopback,DEV=1` を選ぶこと。** snd-aloop は再生側と録音側で
パラメータが一致していないと開けず、PipeWire が何で開くかはこちらから
決められない。`plughw:` なら ALSA の plug 層が差を吸収する。

### 手元でも聴きたい場合

SoundNet 自身で分岐させる。キャプチャ共有（`docs/capture-sharing.md`）が
入っているので、同じ入力から2本のルートを張れる:

- ルート1: `Loopback` → ローカルの出力デバイス（セルフループ。手元で聴く）
- ルート2: `Loopback` → リモートのマシン（配信する）

PipeWire 側で `hw:Loopback,1,0` を掴ませようとすると SoundNet と取り合いに
なる（ALSA デバイスは排他で、同じサブデバイスは1プロセスしか開けない）。
**分岐は SoundNet 側でやること。**

---

## 無音が続いたときに何が起きるか

2種類の「無音」があり、SoundNet はこれを区別する。

| 状態 | SoundNet の挙動 |
|---|---|
| Chrome が何も鳴らしていない（送信側は生きている） | 何も起きない。次の音は**そのままの音量で**出る |
| 送信側のマシンが落ちた・サービスが止まった | 復帰時に音量ランプ（`fade.rs`）が入る |

この区別は roc の接続数で判定している。受信側が
`roc_receiver_query` の `connection_count` を見て、**接続があれば「無音を
送ってきている」、接続が無ければ「送信側がいない」** と解釈する。

以前は「完全なゼロが続いたら送信側がいない」という代理判定だった。
PC 音源のように**クリップとクリップの間が digital silence になる音源では
これが毎回誤爆し、次のクリップの冒頭2秒がランプで潰れる**。イベントでは
それは保護ではなく障害なので、接続数を見るように変えた。

送信側が本当に落ちた場合のランプは従来どおり残っている。これは
「ノート PC を閉じてしまい、再起動後に爆音が送られた」という実際の事故を
受けて入れた保護なので、外していない。

---

## 遅延について

この経路は PipeWire の quantum（既定 1024 frames ≒ 21ms、設定で 256 まで
下げられる）と snd-aloop のバッファが乗るため、**ハードウェア入力より確実に
遅い**。Chrome の音を配る用途には十分だが、モニタリング用途の数字は出ない。

設計上の注意として、snd-aloop には水晶が無い。キャプチャ側は書き込む側
（＝PipeWire のタイマー）に駆動されるので、「デバイスのクロックが唯一の
クロック」という不変条件は構造としては保たれるが、その実体はソフトウェア
タイマーになる。遠端の DAC とのドリフトは roc のリサンプラが吸収するので
破綻はしない（2台が別々の水晶を持つのと同じ状況）が、roc に入るジッタは
本物のカードより悪い。

---

## イベント前チェックリスト

1. `lsmod | grep snd_aloop` — モジュールがロードされている
2. 何も鳴らさずに**10分放置**してから `wpctl status` — シンクが
   `suspended` になっていない
3. 同じく放置後、SoundNet の health が `ok`（`stalled` でない）
4. 放置後に Chrome で再生 → **冒頭が絞られずに**フルレベルで出る
5. ルートを張った状態で `systemctl --user restart soundnet-engine` →
   数秒で自動復帰する

4 が本番で効いてくる。ここがランプで潰れるなら、接続数による判定が
効いていない（roc のバージョンか、RTCP 制御エンドポイントの設定を疑う）。

---

## まだ実機で確かめていないこと

**この手順は通しで実行されていない。** snd-aloop 自体は
`crates/soundnet-engine/tests/loopback.rs` で使っているが、PipeWire を
挟んだ経路と、上記のサスペンド抑止設定は未検証である。

最初に試した人は以下を記録してほしい:

- WirePlumber のルールが実際に一致した `node.name` / `device.name`
- 放置何分でサスペンドが起きたか（抑止前）／起きなくなったか（抑止後）
- PipeWire の quantum を下げたときの実効遅延
