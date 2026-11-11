# 37. Wayland のシェル

バーや壁紙、通知、ランチャー、ロック画面のようなデスクトップシェルを、AntiWebUI で書けるようにする。QuickShell と同じ用途を想定する。

- **レイヤーサーフェス：** `zwlr_layer_shell_v1` の面を、ウィンドウの一種として開く。
- **ディスプレイごとのスレッド：** ディスプレイごとに UI スレッドを分けられる。考え方はサブウィンドウ（[36](36-multi-window.md)）と同じ。
- **シェル用の機能：** ワークスペース、ウィンドウ一覧、Hyprland などの IPC、Wi-Fi、Bluetooth、電池、音声、メディア、通知、トレイなどをリアクティブな値として読める。
- **クレート：** これらは `anti-webui-wayland-shell` クレートにまとめる。

X11 には対応しない。X11 にレイヤーシェルに当たる仕組みはなく、ドックやストラットで似せても、ロック画面のように本物でないと意味のない部分が残るためである。

## API

```rust
use anti_webui::prelude::*;
use anti_webui::shell::prelude::*;

fn main() {
    Shell::new()                         // 普通のウィンドウを持たない。quit() まで動き続ける
        .namespace("my-shell")           // レイヤーの名前空間の既定値
        .run(|| {
            // ディスプレイごとに一度呼ぶ。ディスプレイが外れると、中で作ったものごと片付く
            spawn_for_each_output(|output| {
                // output を省略すると、囲んでいる for_each_output のディスプレイに出る
                open_layer(
                    LayerOptions::new()
                        .layer(Layer::Top)
                        .anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT)
                        .height(32)
                        .exclusive(Exclusive::Auto),
                    move || bar(output),
                );
                open_layer(LayerOptions::background(), wallpaper);
            });
        })
        .unwrap();
}

fn bar(output: Output) -> Div {
    let ws = workspaces();
    let net = network();
    div().flex().flex_row().items_center().gap(8.0).px(8.0)
        .child(for_each(move || ws.on(&output), |w| w.id(), |w| {
            button(w.name()).selected(w.is_active()).on_click(move |_| w.activate())
        }))
        .child(text(|| local_time(Every::Minute).format("%H:%M")))
        .child(text(move || match net.wifi() {
            Some(ap) => format!("{} {}%", ap.ssid, ap.strength),
            None => "offline".into(),
        }))
}
```

### レイヤーサーフェス

```rust
// 今のスレッドに開く（open_window と同じ）
let osd = open_layer(options, move || volume_osd(audio));      // 'static
// 専用のスレッドで開く（spawn_window と同じ）
let wall = spawn_layer(options, move || wallpaper(path));      // Send + 'static

// LayerHandle は WindowHandle の機能をすべて持ち、さらに面の設定を後から変えられる
osd.set_size(Some(240), Some(48));
osd.set_margin(Margin { top: 48, ..Default::default() });
osd.set_keyboard(Keyboard::Exclusive);  // ランチャーを開いた間だけ文字入力を取る
osd.set_exclusive(Exclusive::None);
osd.set_layer(Layer::Overlay);
osd.close();
```

| `LayerOptions` | 意味 | 既定値 |
|---|---|---|
| `layer` | `Background` / `Bottom` / `Top` / `Overlay` | `Top` |
| `anchor` | 貼り付ける辺（`Anchor` のビット和）。向かい合う 2 辺を指定すると、その方向いっぱいに伸びる | なし（中央） |
| `width` / `height` | 論理 px。`None` は伸ばす（両辺に貼ったときだけ） | 中身の大きさ |
| `fit_content()` | 伸ばさない方向の大きさを中身の max-content で決め、中身が変わると付いていく | 大きさを指定しないとき |
| `exclusive` | `Auto`（貼った辺の向きの自分の大きさ）/ `Fixed(n)` / `None` / `Ignore`（他の面の領域も無視する。-1） | `None` |
| `exclusive_edge` | 角に貼ったときに、どの辺を避けさせるか（v5） | 自動 |
| `margin` | 辺からの余白（論理 px） | 0 |
| `keyboard` | `None` / `OnDemand`（クリックで受け取る）/ `Exclusive`（独占する） | `None` |
| `input_region` | `Full` / `Empty`（クリックをすべて下に通す）/ `Rects(..)` | `Full` |
| `output` | 出すディスプレイ。`None` はコンポジタ任せ | 囲む `for_each_output` のディスプレイ |
| `namespace` | コンポジタが規則に使う名前 | `Shell::namespace` |

- **よく使う形のショートカット：**
  - `LayerOptions::bar(Anchor::TOP, 32)`：三方に貼り、`exclusive: Auto` にする。
  - `LayerOptions::background()`：四方に貼り、`Background` に置く。
  - `LayerOptions::overlay()`：`Overlay` に置き、`input_region: Empty` にする。
- **組み立てたときの検査：** 組み立てた時点で、プロトコル上許されない組み合わせを弾く。ありのまま送ると、コンポジタがプロトコルエラーで接続ごと切ってしまうためである。
  - 片側にしか貼っていないのに大きさが `None`：`fit_content` に置き換え、デバッグビルドでは警告を出す。
  - `exclusive: Auto` なのに貼った辺が決まらない：`None` にして警告を出す。
- **ポップアップ：** レイヤーから出すメニュー、select、ツールチップは、`xdg_surface.get_popup(None)` のあと `zwlr_layer_surface.get_popup` でぶら下げる。既存のウィジェットは手を入れずに動く。
- **背景：** レイヤーの何も描かないところは透明にする。バーやドックは、貼った辺の長方形より小さな形のことが多いため。普通の窓は今までどおり白で、`WindowOptions::background` で変えられる。
- **ウィンドウ操作：** 移動、リサイズ、最大化、最小化、窓の飾りは、レイヤーでは何もしない。`window_decorations()` は `Decorations::Server`（飾りなし扱い）を返し、`title_bar` は出ない。
- **閉じる操作：** コンポジタの `closed`（ディスプレイが外れたときなど）は、すぐに閉じる扱いにする。閉じる前のハンドラは通さない。

### ディスプレイ

```rust
// 今つながっているディスプレイ。読むと追跡される。どのスレッドからでも読める
outputs() -> Vec<Output>

// 抜き差しに付いていく
for_each_output(|output| { ... });        // すべて今のスレッドで
spawn_for_each_output(|output| { ... });  // ディスプレイごとに UI スレッド
current_output() -> Option<Output>        // 囲む for_each_output か、そのスレッドのディスプレイ
```

- **`Output`：** `Send + Sync + Clone`。等しいかどうかはコネクタ名で比べる。

  | フィールド | 内容 |
  |---|---|
  | `name` | コネクタ名。DP-1 など。`wl_output` v4 の `name` |
  | `description` / `make` / `model` | 説明、メーカー、型番 |
  | `position` / `logical_size` | `xdg_output` の論理座標 |
  | `mode` | 物理 px とリフレッシュレート |
  | `scale` | 論理と物理の大きさから求めた値。描画は面ごとの `wp_fractional_scale` に従うので、これは表示用 |
  | `transform` | 回転 |

  中身が変わる値（解像度など）は `output.current()` で読み、読むと追跡される。

- **スレッドの選び方：** 関数を 2 つに分けて、どちらかを必ず選ばせる。`open_window` と `spawn_window`、`open_layer` と `spawn_layer` と同じ名前の付け方である。引数で選ぶ形（`OutputThreads::Same` / `Own`）は、`f` に求める型（`Send` かどうか）が違うので、1 つの関数にできなかった。

  | | `for_each_output` | `spawn_for_each_output` |
  |---|---|---|
  | `f` を動かすスレッド | 呼んだスレッド | ディスプレイごとに新しい UI スレッド |
  | `f` の型 | `Fn(Output) + 'static` | `Fn(Output) + Send + Sync + 'static` |
  | ディスプレイの間で状態を持つ方法 | 普通の `signal` | `shared_signal`、または下のサービス |
  | 向くもの | 軽いバー | 重い壁紙（動画、シェーダ）、ディスプレイの多い環境 |

- **`spawn_for_each_output` の細かい動き：**
  - `f` の中の `open_layer` は、そのディスプレイのスレッドに開く。
  - さらに分けたい面だけ `spawn_layer` にすると、その面専用のスレッドで動く。
  - ディスプレイが外れると `f` の Owner を破棄するので、中で開いた面が閉じ、スレッドも終わる。挿し直すと、`f` がまた呼ばれる。

### シェルアプリ

```rust
Shell::new()
    .namespace("my-shell")
    .settings(|s| ...)                     // App と同じ
    .run(|| { ... })                       // FnOnce()。ビューは返さない
    -> Result<(), ShellError>

quit();                                    // すべてのスレッドの窓を閉じて終わる（core。App でも使える）
```

- **始めるときの検査：** Wayland でない場合や `zwlr_layer_shell_v1` がない場合（GNOME など）は、`ShellError::Unsupported { missing }` を返して終わる。
- **普通の窓：** `Shell` の中でも `open_window` と `spawn_window` は使える（設定画面など）。
- **普通のアプリから：** `App` から `open_layer` を呼んでもよい。X11 で動いているときは、開けずにすぐ閉じた扱いになり（`is_open()` が `false`）、警告を一度出す。

### 時間がかかる操作：`Task`

Wi-Fi の接続のように、数秒かかって失敗もある操作の結果を UI スレッドで受け取るための型である。core に置き、シェルの外でも使えるようにする。

```rust
// サービスの操作が返す
net.connect(&ap, Some(password)).then(move |r| match r {
    Ok(()) => status.set("接続しました".into()),
    Err(e) => status.set(e.to_string()),
});

// 自分で作る：f は裏のスレッドで動き、then は作った UI スレッドで動く
let t: Task<Vec<u8>> = task(move || std::fs::read(path).unwrap_or_default());
t.get();          // Option<T>（T: Clone）。読むと追跡される。終わるまで None
t.is_done();

// サービスの中から完了させる形
let (t, done) = Task::pending();   // done: Completer<T>（Send）
done.complete(value);
```

- **UI スレッドとの関係：**
  - `Task` は作った UI スレッドとウィンドウに属する（`!Send`）。
  - `then` は、作ったときのウィンドウに入ってから動く。
  - そのオーナーが破棄されたら、`then` は動かない。
- **捨てたとき：** `Task` を捨てても、操作は取り消さない。取り消せる操作には `cancel()` を付ける。

## シェル用の機能（サービス）

### 共通の作り

- **呼び方：**
  - 機能ごとに入口の関数（`network()` や `battery()` など）を置く。
  - 返すハンドルは `Send + Sync + Clone` で、どのスレッドから呼んでも同じものを指す。
- **状態の持ち方：**
  - 状態は隠れたバックグラウンドスレッドが持ち、フィールドごとの `SharedSignal` で公開する。
  - Wi-Fi の電波の強さが変わっても、SSID を読んでいるビューまでは走り直さない程度の細かさにする。
- **スレッドの規則：**
  - サービスのスレッドは、最初に使ったときに立つ。ユーザーには見せない（[05](05-rendering.md) の UI スレッドの決まり）。
  - 終わるのは、プロセスの終わりか `quit()` のとき。
- **使えないとき：** 相手（NetworkManager など）がいなければ、`available()` が `false` を返し、状態は空になる。相手が後から起きれば、そこで埋まる。
- **操作の返り値：** 操作は `Task<Result<_, ServiceError>>` を返す。
- **読む側の例：**

  ```rust
  let battery = battery();
  show(move || battery.available(), move || {
      text(move || format!("{:.0}%", battery.percentage()))
  })
  ```

### 一覧

| feature | 入口 | 相手 | 中身 |
|---|---|---|---|
| （常に） | `outputs` / `for_each_output` | `wl_output` v4、`xdg_output` | 上の「ディスプレイ」 |
| （常に） | `workspaces()` | `ext_workspace_v1`、または下の IPC | 汎用ワークスペース |
| （常に） | `toplevels()` | `wlr_foreign_toplevel_management` v3、または `ext_foreign_toplevel_list` | ウィンドウ一覧、アクティブ化、閉じる、最小化 |
| （常に） | `idle(timeout)` | `ext_idle_notifier_v1` | 操作のない時間の検知 |
| （常に） | `clipboard()` | `wlr_data_control`（`ext_data_control` はあとで） | クリップボードの監視と履歴、書き込み |
| （常に） | `capture` / `thumbnail` | `ext_image_copy_capture_v1`、`hyprland_toplevel_export_v1` | ウィンドウやディスプレイのサムネイル |
| `hyprland` | `hyprland::*` | `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`、`.socket2.sock` | コマンドの送信、問い合わせ、イベント |
| `niri` | `niri::*` | `$NIRI_SOCKET`（JSON） | アクション、問い合わせ、イベントの流れ |
| `sway` | `sway::*` | `$SWAYSOCK`（i3 IPC） | コマンド、問い合わせ、購読 |
| `network` | `network()` | NetworkManager（D-Bus） | Wi-Fi、有線、接続状態 |
| `bluetooth` | `bluetooth()` | BlueZ | アダプタ、デバイス、接続 |
| `battery` | `battery()` | UPower | 電池、周辺機器の電池 |
| `power-profiles` | `power_profiles()` | power-profiles-daemon | 電源プロファイル |
| `media` | `media()` | MPRIS | プレーヤー、曲の情報、再生の操作 |
| `notifications` | `notification_server()` | `org.freedesktop.Notifications` を自分が持つ | 通知を受け取る側（通知デーモン） |
| `tray` | `tray()` | StatusNotifierItem と dbusmenu | トレイのアイコンとメニュー |
| `session` | `session()` | logind、`/sys/class/backlight` | サスペンド、電源断、ロックの合図、画面の明るさ |
| `audio` | `audio()` | PipeWire | 出力と入力の機器、アプリごとの音量 |
| `apps` | `apps()` | XDG の .desktop、アイコンテーマ、`xdg_activation_v1` | アプリの一覧、検索、アイコン、起動 |
| `system` | `system()` | `/proc`、`/sys` | CPU、メモリ、温度、ディスク、ネットワークの速さ |
| `process` | `Process` | 子プロセス | コマンドを動かし、出力を行ごとにシグナルへ |
| （常に） | `lock_session` / `spawn_lock_session` | `ext_session_lock_v1` | 画面ロック |
| （常に） | `local_time(Every)` | タイマー | 分や秒の境目にそろえて進む時計（core の `clock()` は別物で、毎フレーム進む秒数） |

- **feature の束ね方：**
  - Wayland のプロトコルだけで動く機能（ディスプレイ、ワークスペース、ウィンドウ一覧、アイドル、クリップボード）は、feature なしで常に入れる。依存が増えず、使わなければ接続に bind もしないため。
  - IPC（`hyprland`、`niri`、`sway`）は、serde_json が要るので feature で選ぶ。
  - D-Bus の機能は、どれか 1 つでも使うと内部の `dbus` feature が入る。
  - facade の `anti-webui` からは、`wayland-shell` と `shell-<名前>`（例：`shell-hyprland`、`shell-network`）、全部入りの `shell-all` で選ぶ。

### Wayland の機能と接続

- **専用の接続：**
  - Wayland の機能（ディスプレイ、ワークスペース、ウィンドウ一覧、アイドル、クリップボード）は、サービス専用の Wayland 接続を 1 本持ち、隠れたスレッドで回す。
  - UI の接続とは別のクライアントになる。UI スレッドの詰まりと関係なくイベントを受け取れ、バックエンドに機能ごとの分岐を足さずに済む。
- **ディスプレイの照合：**
  - 2 本の接続のあいだで、ディスプレイはコネクタ名（`wl_output.name`）で照らし合わせる。
  - レイヤーを開くとき、UI 側のバックエンドは同じ名前の `wl_output` を自分のキューに bind する。
  - まだ見えていなければ、一度 roundtrip してから探す。それでもなければ、コンポジタ任せで開いて警告を出す。
- **自分の面が要るもの：** アイドル抑止（`zwp_idle_inhibit`）のように自分の面が要るものは、この接続には置かない。バックエンドの面の機能として、あとで足す。

### 汎用ワークスペース

```rust
let ws = workspaces();
ws.all() -> Vec<Workspace>            // 読むと追跡される
ws.on(&output) -> Vec<Workspace>
ws.active(&output) -> Option<Workspace>
ws.create("name", &output) -> Task<Result<(), ServiceError>>
ws.can_create(&output) -> bool
ws.source() -> Option<WorkspaceSource>   // Hyprland | Niri | Sway | Ext

// Workspace は値（Clone + PartialEq + Send）。操作は情報源への参照を持つ
w.id(); w.name(); w.output(); w.is_active(); w.is_urgent(); w.is_hidden(); w.coordinates();
w.can(WorkspaceAction::Activate);        // Activate | Remove | Assign
w.activate(); w.remove(); w.assign(&output);   // -> Task<Result<(), ServiceError>>
```

- **情報源を選ぶ順番：**
  1. 動いているコンポジタの IPC（`HYPRLAND_INSTANCE_SIGNATURE`、`NIRI_SOCKET`、`SWAYSOCK` を見る）。特殊ワークスペースのように、`ext_workspace` より多く取れるため。
  2. IPC がなければ `ext_workspace_v1`。ShojiWM はこれになる。
  3. どちらもなければ `available()` が `false` を返す。
- **`ext_workspace` を先に使う指定：** `Workspaces::prefer(WorkspaceSource::Ext)` でできる。
- **`ext_workspace` の対応づけ：**
  - グループはディスプレイに、状態は `active` / `urgent` / `hidden` に、`capabilities` は `can()` に対応する。
  - 操作は `ext_workspace_manager_v1.commit` でまとめて送る。
- **情報源ごとにできること：**

  | | 表示 | 削除 | 別の出力へ | 作る |
  |---|---|---|---|---|
  | `ext_workspace` | `capabilities` どおり | `capabilities` どおり | `capabilities` どおり | グループの `create_workspace` |
  | Hyprland | `workspace ID`（特殊は `togglespecialworkspace`） | — | `moveworkspacetomonitor` | — |
  | niri | `FocusWorkspace` | — | `MoveWorkspaceToMonitor` | — |
  | sway | `workspace "名前"` | —（空になると消える） | `move workspace to output` | `focus output` のあと `workspace` |

- **ID：** `ext_workspace` は `id` イベントの値（ShojiWM なら `eDP-2:1`）、IPC はそれぞれの数値の ID を文字列にしたもの。
- **コンポジタ固有の機能：** 生の API で直接使う。
  - `hyprland::dispatch("togglespecialworkspace")`
  - `hyprland::clients()`
  - `hyprland::on_event(|e| ...)`

### Hyprland、niri、sway

```rust
hyprland::dispatch("workspace 3") -> Task<Result<()>>
hyprland::keyword("general:gaps_out", "8") -> Task<Result<()>>
hyprland::monitors() / workspaces() / clients() / active_window()   // 読むと追跡される
hyprland::on_event(|e: &hyprland::Event| ...)                        // 今の UI スレッドで受け取る
hyprland::request("version") -> Task<Result<String, ServiceError>>       // hyprctl と同じ
hyprland::request_json("devices") -> Task<Result<Value, ServiceError>>   // "j/" を付けて送る
```

- **値を新しく保つ方法：**
  - 追跡される一覧は、`.socket2.sock` のイベント（workspace、createworkspace、openwindow など）を受けたときに、関係する問い合わせ（`j/clients` など）をやり直して保つ。
  - イベントの名前は型に起こす。知らないイベントは `Event::Other { name, data }` で渡す。
- **Hyprland のイベント：** `workspace` と `workspacev2` のように 2 回送られるものは、情報の多い v2 の側を型にし、もう一方は `Other` にする。続けて届いたイベントには、問い合わせを 1 回だけする。
- **niri：** `niri::request(json!("Outputs"))`、`niri::action(json!({"FocusWorkspace": ...}))`、`niri::workspaces()`、`niri::windows()`、`niri::focused_window()`、`niri::on_event(|e: &Value| ..)` を持つ。
  - アクションは型に起こさず、niri の JSON をそのまま渡す。niri の版ごとに増えるため。
  - 一覧は `EventStream` の流れから組み立てる（最初に全体が届き、あとは差分）。最初の全体を読むまで、最初の読み出しは待つ。
- **sway：** `sway::command("workspace 3")`、`sway::request(GET_OUTPUTS, "")`、`sway::workspaces()`、`sway::tree()`、`sway::on_event(|e: &sway::Event| ..)` を持つ。ワークスペースの一覧は、`workspace` と `output` のイベントのたびに読み直す。
- **JSON の処理：** JSON は serde_json の `Value` で読み、型への写しは手で書く（版によって真偽値が数値で来るなどの揺れを吸収するため）。serde_json はすでに依存ツリーにあり、`anti_webui::shell::serde_json` として再公開する。

### ウィンドウ一覧、アイドル、クリップボード

```rust
let tl = toplevels();
tl.all() -> Vec<Toplevel>      // 開いた順。読むと追跡される
tl.active() -> Option<Toplevel>
tl.can_control()               // wlr なら true、ext_foreign_toplevel_list なら読むだけ
t.title; t.app_id; t.identifier; t.outputs; t.maximized; t.minimized; t.activated; t.fullscreen; t.parent;
t.activate(); t.close(); t.set_minimized(true); t.set_maximized(true); t.set_fullscreen(true);

let away = idle(Duration::from_secs(300));   // 最後のハンドルが消えると見るのをやめる
away.is_idle()                                // 読むと追跡される

let cb = clipboard();
cb.text(); cb.primary(); cb.history();        // 読むと追跡される
cb.set_text("…"); cb.clear_history(); cb.set_history_limit(100);
```

- **アイドル：** `get_idle_notification` を使うので、動画の再生中のように画面を消さないよう頼んでいるアプリがあれば、アイドルにならない。
- **クリップボード：**
  - 中身は、選ばれるたびに別スレッドでパイプから読む。持ち主の書き込みが遅くても、接続のスレッドは止まらない。
  - 履歴は新しい順で、同じものは 1 つにまとめる。既定は 50 件。
  - パスワードマネージャーが `x-kde-passwordManagerHint` を付けたものは、履歴に残さない。
  - `set_text` で置いたものは、このプロセスが終わると消える（Wayland のクリップボードの決まり）。
- **最初の読み出し：** ワークスペース、ウィンドウ一覧、クリップボードは、最初に使ったときに bind する。そのあと `wl_display.sync` の答えを最大 500 ms 待つので、最初に読んだ時点で中身がそろっている。ただしクリップボードの中身だけは、パイプで後から届く。

### ウィンドウのサムネイル

```rust
let opts = CaptureOptions::default().max_size(480, 270).every(Duration::from_millis(500));
let shot = capture(&toplevel, opts);   // 最後のハンドルが消えると撮るのをやめる
shot.picture() -> Option<Arc<Bitmap>>  // 読むと追跡される
shot.size()                            // 縮める前の大きさ（ピクセル）
shot.stopped()                         // ウィンドウが閉じたなど、もう撮れない
shot.source() -> Option<CaptureSource> // Ext か Hyprland
shot.refresh()                         // 今すぐ撮り直す
bitmap_view(move || shot.picture())

thumbnail(&toplevel, opts)             // 上をまとめたビュー。ビューが消えると撮るのをやめる
capture(&output, opts)                 // ディスプレイ全体（Ext だけ）
capture(&client, opts)                 // Hyprland IPC の `Client` のアドレスから（feature `hyprland`）
```

- **情報源：**
  - `Ext`：`ext_image_copy_capture_v1`。ウィンドウは `ext_foreign_toplevel_image_capture_source_manager_v1`、ディスプレイは `ext_output_image_capture_source_manager_v1` で撮る相手を作る。
  - `Hyprland`：`hyprland_toplevel_export_v1`。v2 は wlr のウィンドウのハンドルで、v1 は `hyprctl clients` のアドレスで撮る。
    - XML は hyprland-protocols のもの（BSD-3）をクレートに取り込み、`wayland-scanner` で生成する。
  - 汎用の `Ext` を先に使い、なければ `Hyprland` を使う。`CaptureOptions::prefer` で変えられる。
  - ShojiWM は両方を持つので、両方とも確かめられる。
- **ハンドルの対応：**
  - `Ext` で撮るには `ext_foreign_toplevel_handle_v1` が要る。ところが `toplevels()` は、操作できる wlr を優先している。
  - 両方あるときは、両方を bind する。ext のウィンドウは、app_id と title が同じもの、なければ app_id が同じで先に出たものと組にする。
  - 組になると、wlr 側の `Toplevel` にも `identifier` が入る。
- **バッファ：**
  - 今は `wl_shm`（memfd）だけを使う。ShojiWM はどちらのプロトコルでも XRGB8888 の shm しか出さない。
  - dmabuf をそのままテクスチャにするゼロコピーは、あと（T2）に回す。撮る側で render node のバッファを確保する仕組みと、描画側で BGRA の dmabuf を取り込む仕組みが要る。
  - T2 が効くのは、概観画面のようにライブのサムネイルがたくさん並ぶときだ。
- **変換：**
  - 撮る要求と変換は、隠れたスレッド「anti-webui capture」1 本でまとめて行う。
  - コンポジタからのイベントは接続のスレッドが受けて、このスレッドへ渡す。
  - このスレッドでの処理：
    - BGRX（または BGRA）を、乗算済みの RGBA に直す。
    - `max_size` に収まるよう、面積平均で縮める。core の `Bitmap` は縮小用の段を持たないので、表示の大きさに縮めておくと粗くならない。
    - `y_invert` と `transform` を直す。
    - できたものを `Bitmap` にして `SharedSignal` に置く。
- **間隔：**
  - `every` がなければ 1 枚だけ撮る。
  - `every` があれば、1 枚撮り終えて `every` 経ったら次を頼む。そのあとはコンポジタが、中身が変わるまで待つ。変わらないウィンドウは撮り直さないので、CPU を使わない。
  - 最初の 1 枚は変化を待たない（Hyprland は `ignore_damage`、Ext は最初の capture がそうなっている）。
- **終わり：**
  - 最後の `Capture` が消えると、撮るのをやめて session、frame、バッファを壊す。
  - Ext の `stopped`、または Hyprland で 3 回続けて `failed` が来たら、`stopped()` が true になる。
- **大きさの単位：** `max_size` はピクセルで指定する。論理の大きさに、出力の `scale` を掛けた値を渡す。

### ネットワーク

```rust
let net = network();
net.connectivity() -> Connectivity      // Full | Limited | Portal | None
net.wifi_enabled(); net.set_wifi_enabled(bool) -> Task<..>
net.wifi() -> Option<AccessPoint>       // つながっている AP
net.access_points() -> Vec<AccessPoint> // ssid, strength, security, frequency, known, active
net.scan() -> Task<..>
net.connect(&ap, password: Option<String>) -> Task<Result<(), NetError>>
net.disconnect() -> Task<..>
net.wired() -> Option<Wired>
net.devices() -> Vec<NetworkDevice>     // interface, kind, state, speed, IPv4/IPv6 のアドレス、ゲートウェイ、DNS
net.set_device_enabled(&device, bool) -> Task<..>
net.connections() -> Vec<SavedConnection> // 保存済みの接続：name, kind, ssid, autoconnect, state
net.activate(&c) -> Task<Result<(), NetError>>; net.deactivate(&c); net.forget(&c)
net.set_autoconnect(&c, bool) -> Task<..>
net.settings(&c) -> Task<Result<ConnectionSettings, _>>  // 名前、自動接続、鍵、IPv4/IPv6 の方式とアドレス
net.save_settings(&c, ConnectionSettings) -> Task<..>
net.join_hidden(ssid, security, password) -> Task<Result<(), NetError>>
```

- **接続の仕方：** `connect` は、保存済みの接続があれば `ActivateConnection`、なければ `AddAndActivateConnection` で行う。
- **パスワード：**
  - 保存済みの接続でパスワードが足りないときの入力（シークレットエージェント）は、あとで足す。
  - それまでは `NetError::NeedsSecret` を返す。
- **設定の書き換え：** `save_settings` は `GetSettings` で全部を読み、よく使う項目だけを変えて、`Update2`（フラグ 0：保存先はそのまま）で書き戻す。ほかの設定は残る。秘密（鍵）を含めなければ、保存済みの鍵はそのまま残る。保存済みの鍵は読めない（`key` は常に `None`）。新しい鍵は NetworkManager 自身が保存する（`psk-flags` 0）。
- **アドレス：** `address-data` と文字の `gateway` で書く。DNS は 2 進の `dns` と、NetworkManager が持っていれば `dns-data`（1.48 から）の両方に書く。おかしなアドレスは、送る前に `ServiceError::Failed` で断る。
- **iwd：** iwd を直接使う版はあとで足す。NetworkManager が iwd を下で使っている構成でも、NetworkManager 経由で動く。

### 通知、トレイ、メディア

- **通知（`notification_server()`）：**
  - `org.freedesktop.Notifications` の名前を取り、通知デーモンになる。
  - 名前がすでに取られていれば、`ServiceError::NameTaken` を返す。もう一度呼べば、取り直しを試す。
  - 各通知が持つもの：`id`、`app_name`、`icon`、`summary`、`body`、`actions`、`urgency`、`image`、`expires`。
  - 操作：`n.invoke("default")`、`n.dismiss()`、`server.set_do_not_disturb(true)`。
  - 本文のマークアップは、`rich_text` に渡せる程度（`b`、`i`、`u`、`a`、`img`）に限る。
- **トレイ（`tray()`）：**
  - StatusNotifierHost になる。`org.kde.StatusNotifierWatcher` がいなければ、自分が watcher にもなる。
  - アイテムが持つもの：アイコン（名前またはピクセル）、タイトル、状態、ツールチップ、メニュー。
  - 操作：`item.activate()`、`item.secondary_activate()`、`item.scroll(delta)`。
  - メニューは dbusmenu を読み、標準の部品の `menu` に組み立てる `tray_menu(&item, open)` を用意する。そのため feature `tray` は `anti-webui-widgets` に依存する。
- **メディア（`media()`）：**
  - プレーヤーの一覧と、今のプレーヤーを持つ。
  - 曲の情報、再生状態、`position()` を読める。`position()` は Rate と時刻から補って進める。
  - 操作：`play_pause`、`next`、`previous`、`seek`。

### 音声

```rust
let audio = audio();
audio.default_sink() -> Option<Device>     // name, description, volume, muted
audio.sinks(); audio.sources(); audio.streams()  // streams はアプリごとの音
dev.set_volume(0.5) -> Task<..>; dev.set_muted(true); audio.set_default(&dev)
stream.set_volume(0.5); stream.set_muted(true)
stream.move_to(&dev)                       // 別の機器で鳴らす（録音なら別の機器から録る）
dev.volumes; dev.positions                 // チャンネルごとの音量と位置（FL、FR など）
dev.balance(); dev.set_balance(-0.5)       // 左右のバランス。-1 が左だけ、1 が右だけ
dev.ports; dev.port; dev.set_port(i)       // ヘッドホン、スピーカー、HDMI などのポート
audio.cards()                              // サウンドカード：profiles、profile
card.set_profile(i)                        // 「アナログステレオ」「HDMI」「オフ」など
```

- **音量とバランス：** `volume` はいちばん大きいチャンネルの音量。`set_volume` は各チャンネルの割合を保つので、バランスは変わらない。バランスは PulseAudio と同じく、小さい側が大きい側の何割かで決める。
- **機器の移動：** `move_to` は pipewire-pulse の `move-sink-input` と同じく、`default` メタデータのストリームの `target.object` に機器の `object.serial` を書く。リンクし直すのは WirePlumber で、選んだ機器はアプリごとに覚えられる。
- **カード：** `media.class` が `Audio/Device` のデバイスをバインドし、`EnumProfile`、`Profile`、`EnumRoute`、`Route` を読む。デバイスの info で、パラメータのフラグが変わったものだけを `enum_params` で読み直し、最初の答えで前の値を置き換える。ポートは機器のノードの `device.id` と `card.profile.device` でカードのルートに結ぶ。選ぶときは `Profile` と `Route` のパラメータを `save` 付きで書く。
- **音のレベル（ピーク）：** 読まない。機器ごとに録音のストリームを開いて、バッファを読む必要があるため。

- **アプリごとの音（`AudioStream`）：** アプリ名、`media_name`、`icon_name`、実行ファイル名（`binary`）、用途（`role`、`media.role`。`Event` や `Notification` はシステムの音）、音量、ミュート、録音かどうか、つながる機器（`target`）を持つ。

- **PipeWire の読み込み方：**
  - PipeWire をそのまま使う。libpipewire は動画の FFmpeg（[24](24-video.md)）と同じく、実行時に dlopen で読み込む。ビルドに C のライブラリは要らない。
  - 見つからなければ `available()` が `false` を返す。
- **音量の目盛り：** 音量は WirePlumber と同じく三乗の目盛り（cubic）で見せる。

### アプリ、プロセス、システム

- **アプリ（`apps()`）：**
  - `XDG_DATA_DIRS/applications` の .desktop を読む。`Name[ja]` などのロケールの名前、`NoDisplay`、`OnlyShowIn` に従う。
  - 検索：`apps.search("fire")`。名前、汎用名、キーワードへの部分一致と頭文字で探す。
  - アイコン：アイコンテーマから探したパスを返し、`img()` に渡せる。
  - 起動：`app.launch()` は Exec のフィールドコードを外して、`setsid` で起動する。`xdg_activation` のトークンを渡すので、起動したアプリが前に出る。
- **プロセス：**
  - `Process::new("cmd").args(..).lines()` で、出力の行を `Signal<Vec<String>>` に流す。
  - `.on_exit(|status| ..)` と `.kill()` を持つ。
- **システム：** `system().cpu()` などで読む。読む間隔は指定でき、既定は 1 秒。誰も読まなければ測らない。

### 画面ロック（L4）

```rust
let lock = lock_session(|output| lock_screen(output));   // spawn_lock_session なら出力ごとのスレッド
lock.is_locked();       // コンポジタが locked を返したら true
lock.unlock();          // 認証が通ったら呼ぶ
```

- **作り：**
  - `ext_session_lock_v1` を使う。ディスプレイごとにロック面を開き、`for_each_output` と同じく抜き差しに付いていく。
  - 認証（PAM）はこのクレートでは扱わず、ユーザーに任せる。
- **異常時：** クライアントが落ちてもコンポジタはロックしたままにする。これはプロトコルの決まりである。

## 中の作り：core

- **`WindowOptions` に面の種類を足す：**
  - `role: Role` を足す。`Role` は `Toplevel`（既定）か `Layer(LayerSpec)` か `Lock { output }` のどれか。
  - `LayerSpec` は飾りのないデータで、プラットフォームには依存しない。
  - `open_layer` と `spawn_layer` は、`role` を付けて `open_window` と `spawn_window` を呼ぶ薄い包み。中身はシェルのクレートに置く。
- **`LayerHandle`：**
  - `WindowHandle` を包み、面の設定を変える命令 `Command::Layer(LayerChange)` を送る。
  - 届け方は `SetTitle` と同じ道を使う。
- **窓のない UI スレッド：** `spawn_ui_thread(f)` を足す。
  - `f` をルートオーナーの下で動かす、窓を持たない UI スレッドを立てる。
  - 返り値は `UiThreadHandle { stop() }`。`stop()` はルートを破棄する。
  - スレッドは、窓かルートが残っている間は動き続ける。`spawn_for_each_output` はこれで作る。
  - 設定とフォントの引き継ぎは、`SpawnRequest::start` と同じ形でする。
- **今のディスプレイ：** core にはまだコンテキストの仕組みがない。そこで `current_output` は、シェルのクレートのスレッドローカルで持つ。
  - `for_each_output` では、`f` を呼ぶ間だけ積むスタックで持つ。あとから（ハンドラなどで）開くレイヤーは、`.output(&output)` で指定する。
  - `spawn_for_each_output` では、スレッド全体のディスプレイとして持つ。そのスレッドで開くレイヤーは、いつ開いても同じディスプレイに出る。
- **ルート：** 窓を持たないスレッドの `f` は、面のない窓として扱う（`start_root`）。
  - `INFO.children` もあるので、ルートを止めると、そこで開いた窓とレイヤーが閉じる。
  - `Ui` がないので、スレッドのループが `run_roots` を呼び、チャネルの取り出しと、ルートのタイマーを動かす。待つときは、ルートのタイマーの時刻（`roots_deadline`）までに起きる。
  - フレームのコールバック（`request_frame`）は、ルートでは表示に合わせられないので、求められている間だけ約 120 Hz のタイマーで動かす。ルートで動かす値（テーマの色の移り変わりなど）も、窓の中と同じく動く。止まっていれば起きない。
    - 以前は動かさなかった。そのため、ルートで `animated` を動かすと止まったままになった（shoji-bar-4 のテーマの切り替えで見つかった）。
- **レイヤーの寿命：** `open_layer` は、開いたときのオーナーが破棄されると閉じる。エフェクトの中で開けば、条件が外れたときに閉じる（例のランチャー）。`open_window` にはない決まりで、レイヤーは画面の一部なので、ビューと同じく作った場所に従わせる。
- **`quit()`：**
  - 窓ごとに `Close` を送り、ルートを持つスレッドの `stop()` を呼び、`App` / `Shell` の `run` から戻る。
  - 閉じる前のハンドラは通さない。
- **`Task` と `task`：** `ui_channel` の上に作る。`then` は作ったときのウィンドウを覚えておき、そこに入ってから動く（`NodeRef` と同じ）。

## 中の作り：app

- **`Shell` の入口：** `anti_webui_app::run_shell`（doc(hidden)）を足し、`Shell::run` から呼ぶ。メインスレッドは窓を持たないルートを持って動き続け、`quit()` で終わる。
- **`Thread` の終わり方：** 「窓が残っている間」から「窓かルートが残っている間」に変える。
- **同じスレッドの別の窓の再描画：** 窓 A のハンドラやタイマー、またはルートでシグナルを書くと、窓 B の文書が変わる。ところが、B の再描画を頼む人がいなかった。そこで、ループの最後に各窓の `needs_redraw` を見て、まだ描く予定のない窓だけ `Session::request_redraw` する。描く予定があるかどうかは `Session::redraw_pending` で見るので、フレームのコールバック待ちの間に空回りすることはない。サブウィンドウ（[36](36-multi-window.md)）にも同じ穴があり、これで一緒に直った。
- **中身に合わせた大きさ：**
  - 開く前に、`Ui::measure_content` で両方向の max-content を測り、最初の `set_size` にする。
  - 開いたあとは、フレームの前に `Ui::content_size` を見る。変わっていれば `set_layer_content_size`（`set_size` と commit）を送り、そのフレームは描かずに `configure` を待つ。
  - コンポジタが別の大きさを返しても止まらないよう、同じ大きさは二度頼まない。100 ms 経ったら描く。
- **`Platform` トレイト：**
  - `open` の `Spec` に `role` を通す。
  - X11 の実装は、`Role::Layer` と `Role::Lock` を `Unsupported` で断る。
- **GPU：** 同じスレッドの面は、今までどおり GPU を共有する。`spawn_for_each_output` のスレッドは、spawn したスレッドと同じく自分のデバイスを作る。

  1 つのプロセスがディスプレイの数だけ Vulkan デバイスを持つことになるので、作ったあとにメモリを測る。多ければ、デバイスをスレッド間で共有する案（wgpu の `Device` は `Send + Sync`）に切り替える。

## 中の作り：Wayland バックエンド

- **グローバル：**
  - `zwlr_layer_shell_v1` を bind する（v4 以上。`OnDemand` は v4、`exclusive_edge` は v5 から）。
  - `wl_output` を名前つきで追う（`Session` 全体で、グローバル名 → コネクタ名の表を持つ）。
- **`open_instance` の分岐：** `xdg_surface` と `xdg_toplevel` の代わりに `get_layer_surface(surface, output, layer, namespace)` を呼ぶ。
  - 大きさ、貼り付け、余白、exclusive、keyboard を設定してから最初の commit をする。
  - 最初の `configure` を待ち、`ack_configure` してから描く。
- **`configure`：**
  - 0 の向きは、コンポジタが決めた大きさを使う。
  - `fit_content` の向きは、レイアウトの max-content を `set_size` で送り、次の `configure` を待つ。
  - 中身が変わって大きさが変わるたびに、この流れを繰り返す。
  - 1 フレームの間に何度も変わったら、最後の値だけ送る。
- **`closed`：** `CloseRequested` ではなく、閉じたことを runner に伝える。
- **入力：**
  - `keyboard: None` の面には、キーボードのフォーカスが来ない。
  - `input_region` は `wl_surface.set_input_region` で送る（`Empty` は空の region）。
  - 入力をインスタンスごとに振り分ける今の仕組み（`owns()`）は、そのまま使える。
- **ポップアップ：**
  - `popup.rs` の親が toplevel かレイヤーかで分ける。レイヤーなら `get_popup(None, positioner)` のあと `layer_surface.get_popup(&popup)` を呼ぶ。
  - 自分で位置を決める subsurface のポップアップは、そのまま使える。
- **ロック：** `ext_session_lock_manager_v1.lock` を呼び、ディスプレイごとに `get_lock_surface` を作る。
  - `configure` の扱いはレイヤーと同じ。
  - `locked` か `finished` を受けたら、ロックのハンドルに伝える。

## クレートの形

```
src/anti-webui-wayland-shell/
  src/lib.rs           Shell、open_layer、spawn_layer、LayerOptions の組み立てと検査、prelude
  src/output.rs        outputs、for_each_output、current_output
  src/conn.rs          サービス専用の Wayland 接続とスレッド
  src/workspaces.rs    汎用ワークスペースと情報源の選び方
  src/toplevels.rs  src/idle.rs  src/clipboard.rs
  src/ipc/{hyprland,niri,sway}.rs
  src/dbus/{mod,network,bluetooth,battery,power,media,notifications,tray,session}.rs
  src/audio.rs         PipeWire（dlopen）
  src/apps.rs  src/process.rs  src/system.rs  src/clock.rs
  src/lock.rs
  examples/bar.rs      バー（ワークスペース、時計、Wi-Fi、電池、音量、トレイ）
  examples/launcher.rs ランチャー（Overlay、Exclusive のキーボード、apps の検索）
  examples/notifications.rs
```

- **依存：**
  - `anti-webui-core`、`anti-webui-app`、`anti-webui-widgets`。
  - `wayland-client`、`wayland-protocols`（staging：ext-workspace など）、`wayland-protocols-wlr`。
  - feature ごとに `zbus` と `serde_json`。
- **D-Bus ライブラリ：** zbus にする（2026-11-07 に測って決めた）。
  - 純 Rust で、NetworkManager、BlueZ、dbusmenu などの相手と型どおりにやり取りできる。
  - 機能は `async-io` だけにし、tokio は入れない。
  - 測った重さ（例 `shell_bar`、release、strip 後）：
    - 実行ファイル：20.4 MB から 23.3 MB へ、+2.9 MB。
    - 常駐メモリ：+3 MB。
  - libdbus を dlopen して手で書く案は、軽さでは勝つ。ただ、dbusmenu の入れ子のレイアウトや `a{sv}` を手で組み立てる量に見合わないので、採らない。
  - スレッド：
    - zbus の接続ごとの実行器は使わない（`internal_executor(false)`）。システムバスとセッションバスのどちらも、隠れたスレッド「anti-webui dbus」1 本で回す。
    - ほかに async-io が自分のスレッドを 1〜2 本持つ。
- **重さ：** 依存が増えて重くなるのは、D-Bus の feature を選んだときだけである。レイヤーとワークスペースだけのバーは、Wayland のクレートだけで作れる。

## 環境ごとの違い

| | レイヤー | ワークスペース | ウィンドウ一覧 | ロック |
|---|---|---|---|---|
| ShojiWM | できる（v5） | `ext_workspace` | wlr-foreign-toplevel v3 | `ext_session_lock` |
| Hyprland | できる | Hyprland IPC | wlr-foreign-toplevel | できる |
| niri | できる | niri IPC | wlr-foreign-toplevel | できる |
| Sway | できる | sway IPC | wlr-foreign-toplevel | できる |
| KDE、COSMIC、river | できる | `ext_workspace` があれば | コンポジタによる | コンポジタによる |
| GNOME | できない（`Shell::run` がエラー） | — | — | — |
| X11、Windows、macOS | 対象外 | — | — | — |

- **確かめた範囲：** ShojiWM の列だけを、2026-11-07 に `wayland-info` で確かめた。他の行は各コンポジタの公開情報に基づくので、実装時に確かめ直す。
- **D-Bus の機能：** D-Bus の機能はコンポジタに関係なく、Linux なら動く。

## やらないこと

- X11 への対応（上の理由）。
- 設定ファイルを書き換えたときにその場で入れ替わるライブリロード。QuickShell の QML の売りだが、Rust では別の大きな仕組みが要るので、この設計の外に置く。開発中の再ビルドは、今の `dynamic_linking` で短くする。
- 認証（PAM）、出力の電源（`wlr_output_power`）。必要になったら足す。

## 段階

1. **L1：レイヤーとディスプレイ**
   - core の `Role`、`spawn_ui_thread`、`quit`、`Task`。
   - app の `run_shell`。
   - Wayland のレイヤー面、`wl_output` の追跡、レイヤーからのポップアップ、`fit_content`。
   - シェルのクレートの `Shell`、`open_layer`、`spawn_layer`、`outputs`、`for_each_output`、`spawn_for_each_output`、`local_time`。
   - 例 `shell_bar`（時計、メニュー、中身に合わせた大きさのメモ、キーボードを取るランチャー）。
2. **L2：Wayland 汎用の機能と IPC**
   - サービス専用の接続。
   - `workspaces`（`ext_workspace`）、`toplevels`、`idle`、`clipboard`。2026-11-07 にできた。
   - Hyprland、niri、sway の IPC と、ワークスペースの情報源の自動選択。
   - 例 `bar` にワークスペースを足す。
   - ウィンドウのサムネイル（`capture`、`thumbnail`）。shm だけで、dmabuf のゼロコピーは T2。
3. **L3：D-Bus の機能**
   - 順番：`network`、`battery`、`power_profiles`、`bluetooth`、`media`、`notifications`、`tray`、`session`。
   - 例 `bar` と例 `notifications`。
4. **L4：残り**
   - `audio`（PipeWire）、`apps`、`process`、`system`、`lock_session`。
   - 例 `launcher`。

## 確かめ方

- **core の単体テスト：**
  - `spawn_ui_thread` のスレッドが、ルートの破棄で終わること。
  - `quit()` で `run` から戻ること。
  - `Task::then` が、作ったウィンドウで動くこと。
  - 破棄したオーナーの `then` が動かないこと。
- **Wayland のプロトコルテスト：** 今の小さなコンポジタに `zwlr_layer_shell_v1` を足して確かめる（`wayland-protocols-wlr` の server feature）。
  - 最初の commit の前に設定が届くこと。
  - `configure` と `ack` の順。
  - `fit_content` の `set_size`。
  - `closed` で閉じること。
  - レイヤーの子の `get_popup`。
  - `keyboard` と `input_region`。
- **出力の追跡：** 小さなコンポジタで `wl_output` のグローバルを足し引きし、`for_each_output` が `f` を呼ぶことと、破棄することを確かめる。`Own` ならスレッドが立つことと、終わることも確かめる。
- **サービス：**
  - Hyprland、niri、sway のイベントと JSON の読み取り。
  - ext-workspace の状態の組み立て。
  - .desktop の読み取りとフィールドコードの除去。
  - 通知の本文のマークアップ。
  - 以上はデータで単体テストする。
  - D-Bus の機能は、テスト用に立てた `dbus-daemon --session` に偽のサービスを置いて確かめる。
- **ShojiWM で手で確かめる：**
  - 例 `bar` を 2 枚のディスプレイで動かす。
    - バーの高さぶん、他の窓が避けること。
    - 抜き差しでバーが付いていくこと。
    - `Own` のときにディスプレイごとのスレッドが立つこと（`top -H`）。
    - ワークスペースの切り替え。
    - Wi-Fi と電池の表示。
    - トレイのメニュー。
  - 例 `launcher` で文字入力が取れること。

## 実装の状態（2026-11-07）

- **L4 もできた。これで設計の段階はすべて揃った。**
  - `audio()`（feature `audio`）：
    - PipeWire を `libloading` で dlopen する。ビルドに C のライブラリは要らない。
    - 出力、入力、アプリごとの音、既定の機器。音量は WirePlumber と同じ三乗の目盛りで、ミュートと既定の切り替えもできる。
    - 音量は `channelVolumes` の Props の pod を手で組んで読み書きする。既定の機器は `default` メタデータの `default.configured.audio.*` に書く。
    - PipeWire が再起動したときの繋ぎ直しは、まだない。
  - `apps()`（feature `apps`）：
    - .desktop を読み、ロケールの名前と表示の条件（`NoDisplay`、`OnlyShowIn` など、`TryExec`）に従う。inotify で追従する。
    - 検索は、名前の頭、語の頭、頭文字、汎用名、部分一致、キーワードの順に並べる。
    - アイコンはアイコンテーマの仕様どおりに探す（GTK の設定、kdeglobals、hicolor、pixmaps）。`icon_path(name, size)` は他の名前（トレイ、通知）にも使える。
    - 起動では、Exec のクォートとフィールドコードを処理し、二重 fork と `setsid` で切り離す。シェルの接続で取った `xdg_activation_v1` のトークンを渡す。`Terminal=true` は端末の中で動かす。
    - `App` は facade の `App` とぶつかるので、型名を `Application` にした。
  - `Process`（feature `process`）：
    - 標準出力と標準エラーを行ごとにシグナルへ流す。`keep(n)` で残す行数を決める（既定 1000）。
    - 標準入力への書き込み、`on_line`、`on_exit` を持つ。
    - `kill()` はプロセスグループに SIGTERM を送り、2 秒後に SIGKILL を送る。作ったオーナーが消えても終わる。
  - `system()` / `system_every(d)`（feature `system`）：
    - CPU（全体とコアごと）、メモリ、温度（CPU の温度の推定つき）、ディスク、ネットワークの速さ、負荷、起動からの時間。
    - 隠れたスレッドが、生きているハンドルの中で一番短い間隔で測る。ハンドルがなくなれば止まる。
  - 画面ロック（`lock_session`、`spawn_lock_session`、feature なし）：
    - core に `Role::Lock(出力名)` を足した。Wayland のバックエンドは `ext_session_lock_v1` で、接続ごとに 1 つのロックを持つ。最初のロック面でロックし、最後のロック面を閉じるとロックを解く（`unlock_and_destroy`）。
    - どの UI スレッドも同じ接続を使うので、`spawn_lock_session`（出力ごとのスレッド）もそのまま動く。
    - コンポジタが断った（`finished`）ときは、状態が `LockState::Refused` になり、ロック画面が閉じる。
    - ロック面は xdg のポップアップの親になれない。そのため、ロック画面のメニューはサブサーフェスで出す。
    - ロック面は、作った時点で `configure` が来ている。それを ack するまで commit してはいけない。xdg やレイヤーの面のように、開いてすぐ中身なしで commit すると、smithay（ShojiWM、niri）は `CommitBeforeFirstAck` で接続を切る。そうなるとロックだけが残り、背景色の画面から戻れなくなる。これは最初の手での確認で実際に起き、2026-11-08 に直した。
    - ロック画面は全画面で不透明な 1 枚なので、ShojiWM は合成せずにそのまま画面に出す（direct scanout）。出し始めとやめるときに dmabuf feedback が変わる。そのたびにこちらが、返却前のバッファまで壊して作り直していたため、画面が乱れた。プレゼンタ側で直した（06 の「バッファ」を参照）。全画面のレイヤーでも同じことが起きていた。
    - それでも、NVIDIA ではほかのアプリの画面の断片が出た。Vulkan で確保したバッファは、そのまま画面に出すと壊れるためだ。バッファを GBM で確保するように変えた（06 の「バッファの確保」を参照）。
    - X11 では開けずに断る。認証（PAM）はこのクレートでは扱わない。
  - 例：
    - `shell_launcher`（`--features shell-apps`）：検索、アイコン、矢印キーと Enter で起動。
    - `shell_bar`：
      - 上のバーに「Lock」を足した。時計と「Unlock」ボタンのロック画面を出す（パスワードは聞かない見本）。
      - サービスの表示は下の 2 本目のバーに移し、CPU とメモリ、音量（-、ミュート、+）を足した。
- **L4 で確かめたこと：**
  - Wayland のプロトコルテスト `lock_protocol.rs`：
    - ロックが出力 DP-2 の上で頼まれること。
    - ack より前に commit しないこと。偽のコンポジタも smithay と同じく、ack 前の commit をプロトコルエラーにする。
    - `configure` の大きさと ack。
    - `locked` で状態が Locked になること。
    - 閉じるとロック面を壊し、`unlock_and_destroy` を送ること。
  - PipeWire：この機械の本物を読むテストで、`wpctl status` と一致した。書き込みは、テストが `pactl` で作って消すヌルシンクにだけ行った。そこで音量 0.50（`wpctl get-volume` でも 0.50）とミュートが反映され、本物の既定と音量は変わっていない。
  - アプリ：一時ディレクトリの .desktop で、隠し、ロケール、検索、アイコン、inotify、起動を確かめた。この機械では 97 個のうち 91 個のアイコンが見つかり、「term」で kitty が先頭に来た。
  - プロセス：出力、終了コード、キーの制限、`cat` への入力、`kill()`、オーナーの破棄で止まること。
  - システム：この機械の本物を読んだ（CPU 2%、メモリ 9.1/31.2 GiB、k10temp で 74 °C）。
  - `shell_bar` を `--features shell-all` で動かすと、下のバーに電池、プロファイル、CPU とメモリ、音量、Wi-Fi、Bluetooth、明るさ、トレイが出た。ランチャーにアイコンつきでアプリが並んだ。
- **L4 でまだのもの：**
  - 画面ロック：本物のセッションでは自動で試さない。代わりに、ShojiWM を入れ子で立ち上げてその中でロックした。最小限の設定で、`XDG_RUNTIME_DIR` も分けた。Locking → Locked → Unlocked と進み、ロック画面が画面いっぱいに映ることを grim で確かめた。直す前のコードでは、手で確かめたときと同じく接続を切られた。
  - **本物のセッションでのロックを手でもう一度確かめること。**
  - ランチャーへの文字入力と起動を手で確かめること（ここでは wtype がなく、キーを送れなかった）。
  - PipeWire の再起動への追従、`set_default` を本物で試すこと。

- **L3 もできた。**
  - 共通：`dbus.rs`（zbus。2 本のバスを隠れたスレッド 1 本で回す、`while_owned` で相手が後から起きても埋まる）と、UI スレッドへイベントを渡す `listeners.rs`。
  - 機能（どれも feature。facade では `shell-<名前>`、全部は `shell-all`）：
    - `network()`：接続状態、Wi-Fi のオンオフ、AP の一覧（SSID ごとに一番強いもの、強い順）、スキャン、接続、切断、有線、機器とそのアドレス、保存済みの接続（接続、切断、削除、自動接続、設定の読み書き）、隠しネットワーク。保存済みでない鍵つきの AP をパスワードなしでつなぐと `NetError::NeedsSecret`。保存済みでない Enterprise は `Unsupported`。
    - `battery()`：表示用のまとめた電池、`devices()` でマウスなどの電池も。残り時間、電力、エネルギー、劣化具合（`health()`）、充電回数（`charge_cycles()`）も読める。後の 2 つは、表示用の電池にはないので、パソコン自身の電池から取る。
    - `power_profiles()`：新旧どちらの名前の power-profiles-daemon にも対応。
    - `bluetooth()`：アダプタと機器、電源、探索、接続、ペアリング、信頼、削除、機器の電池。
    - `media()`：MPRIS のプレーヤーの一覧と今のプレーヤー、曲の情報、操作。`position()` は追跡されない計算で、UI は自分のタイマーで読む。
    - `notification_server()`：通知デーモンになる。`popups()` と履歴、操作、期限、`replaces_id`、おやすみモード（緊急のものは出す）。本文は `b`、`i`、`u`、`a`、`img` だけ残し、`body_text()` で文字だけも取れる。
    - `tray()`：StatusNotifierHost。watcher がいなければ自分がなる。アイコン（名前とピクセル）、ツールチップ、操作、dbusmenu のメニュー、`tray_menu`。
    - `session()`：サスペンド、休止、電源断、再起動、ロック、ロックとスリープの合図、画面の明るさ（logind の `SetBrightness`、`actual_brightness` を inotify で追い、値は `brightness` から読む。`actual_brightness` はハードウェアが実際に出している値で、amdgpu などでは頼んだ値より低いか、まだ変化の途中なので、スライダーが置いた位置から戻ってしまう）。
  - 例 `shell_bar` を `--features shell-all` で作ると、電池、電源プロファイル、Wi-Fi、Bluetooth、明るさ、再生中の曲、トレイが出る。通知デーモンになれたときは、右上に通知が出る。
- **L3 で設計から変えたところ：**
  - feature 名は Cargo の流儀に合わせ、`power-profiles` にした。
  - `network` の `NetError` 以外の操作は、どれも `ServiceError` を返す。
  - 明るさの置き場所は、テストのために `ANTIWEBUI_BACKLIGHT_DIR` で変えられる（文書には載せない）。
- **L3 で確かめたこと：**
  - 自分専用の `dbus-daemon` に偽のサービスを立てたテスト（`tests/*_bus.rs`、9 本）。読み出し、`PropertiesChanged` への追従、操作が相手に届くこと、相手が消えると空になり、戻ると埋まること。3 回続けて通った。
  - この機械の本物（読むだけ）：UPower（80%、充電待ち）、power-profiles-daemon（balanced）、NetworkManager（TP-Link_09BC_5G、59%）、BlueZ、logind（明るさ 0.39）、トレイ（Fcitx と Discord のアイコンとメニュー）が、バーに正しく出た。
  - 通知は、本物のバスでは quickshell が名前を持っているので `NameTaken` になった。`dbus-run-session` の中で `notify-send` を 2 つ送り、右上に新しい順で出て、マークアップが除かれることを確かめた。
- **L3 でまだのもの：**
  - 手で確かめること：Wi-Fi の接続と切断、Bluetooth の接続、トレイのメニューのクリック、メディアの操作（どれも本物の状態を変えるので、自動では試していない）。
  - シークレットエージェント（保存済みの接続でパスワードが足りないときの入力）。
  - トレイのメニューの `icon-data`（PNG）。core に公開の PNG 読み込みがないので、今は `None`。
  - 誰かが watcher のときのトレイのテストは、本物のバス（読むだけ）でしか確かめていない。

- **ウィンドウのサムネイルもできた。**
  - `capture`、`thumbnail`、`CaptureOptions`（`max_size`、`every`、`cursor`、`prefer`）。情報源は `Ext`（ウィンドウとディスプレイ）と `Hyprland`（wlr のハンドル、または IPC のアドレス）。
  - `toplevels()` は wlr と ext の両方があれば両方を bind し、組にする。wlr 側の `Toplevel` にも `identifier` が入る。
  - Hyprland のプロトコルは `protocols/hyprland-toplevel-export-v1.xml` から `wayland-scanner` で生成する。
  - 例 `shell_bar` に「Windows」ボタンを足した。押すと、全ウィンドウのライブのサムネイルが並ぶ概観を開く。クリックすると、そのウィンドウをアクティブにする。
    - `ANTIWEBUI_CAPTURE=hyprland` で Hyprland 側を使う。
    - `ANTIWEBUI_SHELL_OVERVIEW=1` で、最初から開く。
  - 確かめたこと：
    - ShojiWM で `Ext` と `Hyprland` の両方から絵が取れた（`tests/capture_live.rs`、`--ignored` で動かす）。1703×1616 のウィンドウが 320×304 になり、最初の 1 枚まで約 140 ms。
    - 概観に 3 つのウィンドウが正しい向きと色で出た。250 ms ごとに更新しても、各スレッドの CPU は 1% 前後。
    - 単体テスト：色の並べ替え、面積平均での縮小、回転（Weston の `weston_transformed_coord` に合わせた）、`y_invert`。
  - まだのもの：
    - dmabuf のゼロコピー（T2）。
    - 回転したディスプレイの撮影を実機で確かめること。
    - ディスプレイの撮影の `wlr_screencopy` 版。
    - 本物の Hyprland での確認。

- **L2 もできた。**
  - サービス専用の接続に、どのスレッドからでも bind と同期待ちができる `Link` を足した。
  - `workspaces`（`ext_workspace_v1`、Hyprland、niri、sway、情報源の自動選択と `prefer`）、`toplevels`（wlr、ext）、`idle`、`clipboard`（wlr）。
  - `hyprland`、`niri`、`sway` の IPC（feature）。facade では `shell-hyprland`、`shell-niri`、`shell-sway`、`shell-all`。
  - 例 `shell_bar` に、ワークスペースのボタン、キーボードを持つウィンドウの名前、アイドルを足した。
- **L2 で設計から変えたところ：**
  - Wayland の機能に feature を付けなかった（上の「feature の束ね方」）。
  - 作れるかどうかは、`ws.can(WorkspaceAction::Create)` ではなく `ws.can_create(&output)` で聞く。作れるかどうかは出力ごとに違うため。ほかの操作は `w.can(..)` で聞く。
  - Hyprland の生の問い合わせは、型引数を取らず `request`（文字列）と `request_json`（`Value`）にした。
  - niri のアクションは型にせず、JSON で渡す。
- **L2 で確かめたこと：**
  - ShojiWM で、次のものが値を返した。
    - ワークスペース：情報源は `Ext`。`eDP-2:1` が active で、出力と座標つき。
    - ウィンドウ一覧：wlr で操作もできる。kitty と Discord が状態つきで出た。
    - クリップボード：読み、書き、履歴。
    - アイドル：1.5 秒で idle になった。
  - バーにワークスペースのボタンと、キーボードを持つウィンドウの名前が出た。
  - 偽のサーバーを立てたテスト（`tests/hyprland_ipc.rs`、`niri_ipc.rs`、`sway_ipc.rs`）で、3 つの IPC について次を確かめた。
    - 最初の読み出し。
    - イベントに付いていくこと。
    - ワークスペースの表示（Hyprland では特殊ワークスペースも）が正しいコマンドになること。
    - `on_event` が UI スレッドに届くこと。
  - 単体テストで、Hyprland のイベントと返答の読み取り、niri のイベントの流れ、i3 IPC の枠を確かめた。
- **L2 でまだのもの：**
  - `ext_data_control`（KDE などの新しいほう）。今は wlr のクリップボードだけ。
  - 本物の Hyprland、niri、sway での確認。
  - ShojiWM ではワークスペースが 1 つしかなく、表示の切り替えを試せていない。

- **L1 はできた。**
  - core：`layer`（`LayerSpec`、`Anchor` など）、`Role`、`spawn_ui_thread`、`quit`、`Task`、ルート（`start_root`、`run_roots`）、`Ui::measure_content` と `content_size`。
  - app：`run_shell`、ルートのあるスレッド、同じスレッドの別の窓の再描画、中身に合わせた大きさ。
  - Wayland：`layer.rs`、レイヤーからのポップアップ、名前つきの `wl_output`。
  - X11：レイヤーは開けずに断る。
  - シェルのクレート：`Shell`、`LayerOptions`（組み立てと検査）、`LayerHandle`、`outputs`（専用の接続）、`for_each_output`、`spawn_for_each_output`、`local_time`。
  - facade の feature は `wayland-shell` で、`anti_webui::shell` として使う。例は `shell_bar`。
- **設計から変えたところ：**
  - ディスプレイごとのスレッドは、引数ではなく関数で選ぶ（`for_each_output` / `spawn_for_each_output`）。
  - 辺の型を `Edges` から `Anchor` にした。スタイルの `Edges<T>`（余白など）と、prelude で名前がぶつかったため。
  - 時計を `clock` から `local_time` にした。core の `clock()` とぶつかったため。
- **確かめたこと：**
  - テスト：core（ルート、中身に合わせた大きさ、`Task`、`LayerSpec`）、シェルのクレート（`LayerOptions` の検査、`strftime`）。Wayland のプロトコルテスト（`layer_protocol.rs`）では、最初の commit の前の設定、出力の名前、`configure` と大きさ、中身の大きさと zone、変更、`closed` を確かめた。
  - ShojiWM（eDP-2、スケール 1.8）：
    - バーが専用のスレッドで出て、他の窓がバーの高さぶん避けること。
    - メモが中身に合わせて広がること。
    - ランチャーが前面に出て、文字入力欄にフォーカスが来ること。
    - `quit()` で `run` から戻ること。
    - `for_each_output` でも同じに動くこと。
- **まだのもの：**
  - 手で確かめること：メニューのポップアップ、クリックとキー入力、ディスプレイの抜き差し、2 枚目のディスプレイ。
  - 出力の追跡のテスト（`wl_output` の足し引き）。

## 決まっていないこと

- `spawn_for_each_output` のスレッドが自分の Vulkan デバイスを持つか、共有するか。ディスプレイが 1 枚の環境では差が出ないので、2 枚以上でメモリを測ってから決める。
