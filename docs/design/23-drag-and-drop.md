# 23. つかみと DnD

要素をつかんで動かす操作と、ドラッグ＆ドロップを 3 つの層に分ける。

| 層 | 用途 | API | 動く範囲 |
|---|---|---|---|
| 1. つかみ | パネル移動、スライダー、シートを引く | `.on_grab(...)` | 要素自身 |
| 2. アプリ内 DnD | カンバン、ツリーの移動 | `.draggable(...)`、`.drop_target(...)` | 窓の中 |
| 3. OS の DnD | ファイルの受け取り、他アプリへの持ち出し | 2 と同じ API に外向きのデータを足す | デスクトップ全体 |

並べ替えの部品（`sortable_list`、`sortable_row`）は第 1 層の上に作る。

## 第 1 層: つかむ

```rust
let pos = signal((40.0f32, 40.0f32));
let from = Cell::new((0.0f32, 0.0f32));

div()
    .cursor(CursorIcon::Grab)
    .dyn_style(move |s| { let (x, y) = pos.get(); s.translate(x, y) })
    .on_grab(move |g| match g.phase {
        GrabPhase::Start => from.set(pos.get_untracked()),
        GrabPhase::Move => {
            let (x, y) = from.get();
            pos.set((x + g.total.x, y + g.total.y));
        }
        GrabPhase::End => {}
        GrabPhase::Cancel => pos.set(from.get()),
    })
```

### `GrabEvent`

| フィールド | 単位 | 内容 |
|---|---|---|
| `phase` | | `Start`、`Move`、`End`、`Cancel` |
| `total` | 論理 | 押した場所からの移動量 |
| `delta` | 論理 | 前のイベントからの移動量 |
| `velocity` | 論理/秒 | 最近の動きの速さ |
| `position`、`local`、`rect` | 物理 | `PointerEvent` と同じ |
| `modifiers` | | 修飾キー |

### 決まりごと

- **遊び。** 既定は 4 論理ピクセル（ダブルクリックの判定と同じ）。動くまではクリックのまま。遊びを越えたときは `Start` のすぐあとに `Move` が来るので、`Move` だけ見ていても動きを失わない。`.grab_threshold(0.0)` なら押した瞬間に `Start` だけが来る。
- **クリック。** つかんで遊びより大きく動かしたら、離しても `click` は来ない。
- **文字選択。** つかめる要素の中では、押しても文字選択を始めない。テキスト入力とスクロールバーはつかみより優先する。
- **入れ子。** 押した要素から祖先へ辿り、最初につかめる要素が受ける。
- **取り消し。** つかんでいる間の Esc と、窓がフォーカスを失ったときに `Cancel` が来る。`Cancel` のあとは離すまで何も来ない。Esc は部品に届かない。
- **カーソル。** `cursor(Grab)` の要素は、押している間 `Grabbing` になる。
- **要素が消えたら。** 何も呼ばない。要素の持ち主の `Signal` は破棄されていて、読むと panic するためである。
- **`.grab_autoscroll()`。** 窓やスクロール領域の端で自動スクロールし、そのたびに `Move` を送る。

`cancel_press` が今も `on_pointer_up` と `on_active(false)` を呼ぶのは変えない。区別が必要なものは `on_grab` を使う。

## 第 2 層: アプリ内 DnD

同じプロセスの中なので、値を文字列にせず Rust の型のまま渡す。受け手は型で選ばれる。

```rust
#[derive(Clone)]
struct CardDrag { id: u64, from: ColumnId }

div()
    .draggable(move || CardDrag { id, from })
    .dragging(|s| s.opacity(0.4))

div()
    .drop_target(move |d: &CardDrag, e: &DropEvent| move_card(d.id, col, e.local.y))
    .drag_over(|s| s.bg(hover))
```

| API | 内容 |
|---|---|
| `.draggable(f)` | 動き出した瞬間に `f` を呼ぶ。`T` か `DragData` を返す |
| `.dragging(style)` | 運ばれている間の元の要素のスタイル |
| `.drag_preview(f)` | プレビューを自分で作る |
| `.on_drag_end(f)` | 終わった結果（`Moved`、`Copied`、`Cancelled`）。元の要素が消えていれば呼ばれない |
| `.drop_target(f)` | 型 `T` を受ける。型を変えて何度でも書ける |
| `.drop_accepts(f)` | 型 `T` のうち受けるものを絞る |
| `.on_drag_over(f)` | 型 `T` が重なっている間の位置 |
| `.on_drag_hover(f)` | 何かを受けられる状態で重なり始めた / 終わった。ドロップのあとも `false` が来る |
| `.drag_over(style)` | 同じ間のスタイル |
| `dragged::<T>()` | 運ばれている `T`。読むと追跡する |

- **受け手の決め方。** ポインタの下の要素から祖先へ辿り、最初に受け入れる要素だけが受ける。HTML の `dragenter` / `dragleave` のように子要素で揺れない。`preventDefault` も要らない。
- **動作。** `DragAction::Move` が既定で、Ctrl を押していると `Copy`。`DragData::actions` で元が許すものを絞れる。
- **運んでいる状態は `Ui` が持つ。** 元の行が `virtual_list` のスクロールで消えても続く。元の要素を指すのは見つかる間だけである。
- **ホバーと押下。** 動き出すと `:active` と `:hover` は外れ、終わるとポインタの場所で計算し直す。Chrome も運んでいる間はホバーを更新しない。元の要素に `on_pointer_up` は来ない（HTML の `mouseup` と同じ）。
- **自動スクロール。** スクロール領域の端から 40 論理ピクセル以内に 200 ms 留まると、端に近いほど速く動く。入った瞬間に動かないので、端の行をつかんでもすぐには流れない。

### プレビュー

| | 既定 | `.drag_preview(f)` |
|---|---|---|
| 中身 | 動き出す時点の元の要素を描いた `Scene` | `f` の作ったノード |
| 不透明度 | 0.75（Blink の `kDragImageAlpha`） | 1.0 |
| 更新 | しない | 普通のノードと同じ |
| ポインタの位置 | つかんだ場所 | 元の要素でつかんだ割合と同じ場所 |

どちらも窓の `Scene` の最後に `Primitive::Layer` として置く。レイヤーの内容は共有したまま `origin` だけが変わるので、動いている間はテクスチャの合成だけで済み、再レイアウトも再描画も起きない。論理ピクセルを介さないので常に物理ピクセルの上に乗り、文字がぼけない。

`f` は元の行の持ち主の外で動くので、行だけが持つ `Signal` を読まないこと。行が消えたあとに読むと panic する。

## 第 3 層: OS の DnD

**外へ出せるデータを足すと OS の DnD になる。** 型だけならアプリ内のままである。

```rust
.draggable(move || {
    DragData::new(CardDrag { id, from })
        .text(title.clone())
        .files([path.clone()])
})
.on_drag_end(move |r| if r == DragResult::Moved { remove(id) })

.on_drop_files(|files: &[PathBuf], e| open_all(files))
.on_drop_text(|s: &str, e| insert(s))
.on_drop_data("application/x-thing", |bytes: &[u8], e| load(bytes))
```

| | 内部（型だけ） | OS（外向きのデータあり） |
|---|---|---|
| 入力 | ポインタイベント | `wl_data_device` |
| プレビュー | 窓の `Scene` | アイコンのサーフェス |
| 窓の外 | 見えない | 見える |
| Esc | core | core が source を破棄する。コンポジタが処理すればそれでもよい |
| フォーカス喪失 | 取り消す | コンポジタに任せる |

運ぶ側と受ける側のコードは変わらない。

動き出す時を自分で決めるものは、押している間に `start_drag(data, grip, preview)` を呼ぶ。`draggable` と同じ扱いになる。ボタンを離していれば何もしない。使っているのは PDF ビュワーの[文字を運ぶ](26-pdf.md#文字を運ぶ)操作である。

- **`.files` の既定は `Copy` だけ。** ファイラが `Move` を選ぶと元のファイルが消えるからである。`.actions(...)` で変えられる。
- **自分の窓に落としたとき。** データは読まずに手元の値を使う（クリップボードと同じ）。
- **同じスレッドの別の窓に落としたとき。** 運んでいる窓が値・型・形式をスレッドに残しておき、受ける窓は同じ形式の入場ならその値として扱う（`ExternalDrag` にしない）。シェルが別のサーフェスに置いたパネルからバーへ運ぶ場合などである。値を `dragged` から消すのは、運び始めた窓が終わったときだけ。
- **外から来たとき。** 重なっている間は種類しか分からない。`dragged::<ExternalDrag>()` と `.on_drag_over(|x: &ExternalDrag, e| ...)` で知ることができる。ドロップ後に別スレッドで読み、届いてからハンドラを呼ぶ。
- **対応しないプラットフォーム。** `wl_data_device_manager` が v3 未満なら内部モードで動く。外向きのデータはアプリ内の `on_drop_text` などには届く。

### Wayland の流れ

```
遊びを越える
  core:    StartDrag（MIME とデータ、アイコンの大きさ）
  wayland: wl_data_source を作り、MIME を offer、set_actions、
           アイコンの wl_surface を作り、start_drag(source, 押した面, icon, 押下の serial)
  以後は wl_pointer ではなく wl_data_device にイベントが来る

enter / motion / leave → WindowEvent::DragEntered / DragMoved / DragLeft
  core: 受け手を決めて AcceptDrag(MIME, 優先する動作)
        wayland: wl_data_offer.accept、set_actions（変わったときだけ）

drop → WindowEvent::DragDropped
  自分から: core がハンドラを呼び、FinishDrop → wl_data_offer.finish
  他から:   ReceiveDrop(MIME) → 別スレッドで読む → DropData → ハンドラ → finish

wl_data_source.dnd_finished / cancelled → WindowEvent::DragSourceEnded(結果)
  core: on_drag_end を呼び、CloseDragIcon → presenter が忘れてから面を壊す
```

- `start_drag` の serial は、ボタンの押下のものを別に持つ。今の `press_serial` はキーでも変わるので、ドラッグ中に Ctrl を押すと合わなくなる。
- `drop` のあとに `leave` を送るコンポジタがある。`finish` するまで offer を壊さないように、`drop` のあとの `leave` は無視する。
- アイコンのサーフェスは次のフレームを止めない。コンポジタは見えていない面にフレームコールバックを送らないことがある。

## 選択した文字のドラッグ

Chromium と同じく、選択範囲を押して動かすと文字を運べる。追加の API はなく、文字選択とテキスト入力が最初からこの動作をする。Blink の該当コード（`drag_controller.cc`、`selection_controller.cc`、`mouse_event_manager.cc`）を読んで合わせた。

### 押したとき

| 操作 | 動作 | Blink |
|---|---|---|
| 選択範囲の上を 1 回押す | 選択を崩さず、動くのを待つ | `mouse_down_was_single_click_in_selection_` |
| どちらかの軸で 4 論理ピクセル動く | 運び始める。クリックは来ない | `kDragThresholdX/Y`（Mac 以外は 4）、`InvalidateClick` |
| 動かさず離す | 文書の選択は消え、入力欄は押した所にキャレットを置く | `HandleMouseReleaseEvent` |
| Shift を押しながら、2 回目以降のクリック | 今までどおり選択を伸ばす、単語を選ぶ | `!extend_selection`、シングルクリックだけ |
| パスワード欄 | 運ばない。普通の押下になる | `IsInPasswordField` |
| `draggable`、`on_grab` の中の選択範囲 | 選択範囲が勝つ | `DraggableNode` が先に選択を見る |

「選択範囲の上」はハイライトの矩形の中である。Blink は位置の比較なので行末の右の余白も含むが、見えているものを基準にする。

離す条件だけ Blink と変えた。Blink は押した座標と 1 ピクセルでも違うと選択を残すが、ここでは遊びの中ならクリックとして扱う。ほかのクリックの判定と同じ基準である。

Blink はカーソルを選択範囲の上でも I ビームのままにするので（`ShouldShowIBeamForNode`）、変えない。

### 画像

`kSelectionDragImageOnly` と同じものを描く。

- **選択された文字だけ。** クラスタ単位で切り出すので、斜体のはみ出しも切れない。選択されていない文字、ハイライト、背景、箱は描かない。
- **色は文字そのものの色。** `::selection` の色は使わない（`highlight_style_utils.cc`）。
- **不透明度は 0.75。** `kDragImageAlpha`。
- **見えている部分だけ。** スクロールで隠れた部分と窓の外は描かない（`ClipByVisualViewport`）。Ctrl+A のあとでも画像は窓より大きくならない。
- **押した文字がポインタの下に留まる。** 画像は元の位置から動かした分だけずれて見える。

既存のプレビューと同じ `Scene` で、OS モードではアイコンのサーフェス、内部モードではレイヤーとして出す。回転や拡大のかかった文字は、位置だけ合わせて変形なしで描く。

### 動作とデータ

| 元 | 許す動作 | 外へ渡す形式 |
|---|---|---|
| 文書の文字、読み取り専用の入力欄 | `Copy` | `text/plain;charset=utf-8`、`UTF8_STRING`、`text/plain` |
| 編集できる入力欄 | `Move`、`Copy`（Ctrl で `Copy`） | 同上 |

- **他のアプリへ `Move` しても元の文字は消さない。** Blink は `DragSourceEndedAt` で元を消さない。消すのは同じ窓の入力欄へ移すときだけである（`DragIsMove`）。
- **値は `DraggedText`。** `.drop_target(|t: &DraggedText, e| ...)` や `dragged::<DraggedText>()` で受けられる。`.on_drop_text` にも届く。
- `text/html` は出さない。文字列しか持っていないためである。

### テキスト入力へのドロップ

入力欄は最初から文字を受ける。`read_only` と無効な入力欄は受けない。

- **重なっている間。** ポインタの下の文字位置にキャレットを出す（点滅しない）。複数行の入力欄は端で自動スクロールする。
- **落としたとき。** その位置に挿入し、挿入した文字を選択して、入力欄にフォーカスを移す（`kSelectReplacement`）。文書の選択は消える。`value` と `on_input` は普通の入力と同じように動く。
- **同じ入力欄の中で動かす。** 取り消し 1 回で戻る。自分の選択範囲の上には落とせない（`CanProcessDrag`）。
- **別の入力欄へ動かす。** 元を消して先に挿入する。取り消しはそれぞれの入力欄に 1 回ずつ。
- **他のアプリから。** 希望する動作は `Copy`。Blink も外からの文字には `kCopy` を返す。データは別スレッドで読み、届いてから挿入する。
- **単一行の入力欄。** 改行は空白になる（貼り付けと同じ）。
- **`.on_drop_text` などを同じ入力欄に付けた場合。** そちらが勝つ。

## 並べ替え

```rust
sortable_list(items, |t| t.id, |t, row| {
    div().flex().child(row.grip(text("≡"))).child(text(t.title))
})
```

- `items: Signal<Vec<T>>`。離したときに並びを書き換える。取り消すと何も変わらない。
- `row.grip(el)` を使うとそこだけがつかめる。使わなければ行全体。`row.grabbed()` はつかまれている間 `true`。
- `Alt+↑`、`Alt+↓`（`sortable_row` は `Alt+←`、`Alt+→`）で動かす。HTML の DnD にはキーボードの手段がない。
- **並びはつかんでいる間 `translate` だけで見せる。** レイアウトは変えず、離したときに 1 回だけ並べ直す。スクロール中の行の増減がないので行数に比例するのは開始時の計測だけである。
- 高さの違う行でも動く。間隔は開始時に測る。
- 長いリストに必要な `virtual_list` との組み合わせはやらない（行の位置を測れないため）。

標準の部品は 120ms の移動、MD3E はばねと影で持ち上げる。どちらも `anti-webui-widgets` の `sortable_with` を使う。

## core に足すもの

| 追加 | 内容 |
|---|---|
| `src/drag.rs` | 型、`Ui` の処理、自動スクロール |
| `Listeners.gesture`、`Listeners.drop` | `Box` なので使わないノードは 2 ワードだけ |
| `NodeStyle.dragging`、`drag_over` | `:active` より上の層 |
| `WindowState.dragging`、`drag_over` | 今の元と受け手 |
| `Document.drag` | プレビューの描き方 |
| `Frame.drag_icon` | OS モードのアイコンの `Scene` |
| `WindowEvent`、`PlatformRequest` の追加 | 上の流れ |
| `CursorIcon::Copy` | `Copy` の間のカーソル |

## 既存の部品の置き換え

手書きだった 8 か所を `on_grab` にした: `split_row` / `split_col`、`slider`、`table` の列幅（標準）、`slider`、`time_picker`、`carousel`、`bottom_sheet`、`side_sheet`（MD3E）。

変わった動作は次のとおり。

- `carousel` を動かしたあと、中の項目に `click` が来なくなった。
- `bottom_sheet` と `side_sheet` は 4 論理ピクセル動いてから動き出す。Esc やフォーカス喪失で元の位置へ戻る（閉じない）。
- `side_sheet` の中の文字はドラッグで選択できなくなった。シート全体がつかめるからである。
- `slider`、`time_picker` は Esc でドラッグが終わるが、値は戻さない（Chrome の `<input type=range>` と同じ）。

## 測ったもの

`cargo run --release -p anti-webui --example bench_drag`。2340×1616、拡大率 1.8、ウィンドウなし。ポインタを 600 回動かし、そのたびに変わった所だけを描く。GPU は `Gpu::headless` が選んだ AMD Radeon 880M（内蔵）で、GPU の時間は提出から完了までである。単位はミリ秒、2 回の計測の範囲。

| 内容 | UI p50 | UI p99 | UI 最大 | GPU p50 | GPU p99 | 再描画（p50） |
|---|---|---|---|---|---|---|
| カード 592 枚の上を運ぶ | 0.23〜0.24 | 0.48〜0.50 | 0.68〜0.70 | 0.28 | 3.2〜3.4 | 0.4% |
| 同じ、自作プレビュー | 0.23〜0.24 | 0.46〜0.48 | 0.47〜0.87 | 0.32 | 0.49〜2.1 | 1.4% |
| `sortable_list` 50 行 | 0.03〜0.04 | 0.08〜0.10 | 0.17 | 0.25〜0.29 | 0.51〜0.62 | 8.1% |
| `sortable_list` 500 行 | 0.04 | 0.08〜0.14 | 0.49〜0.69 | 0.25〜0.28 | 0.44〜0.86 | 8.1% |

`sortable_list` は離したときに 1 回だけ並べ直す。そのときの `update` は 50 行で 0.14 ms、500 行で 0.24〜0.30 ms。

選択した文字のドラッグは、Ctrl+A のあとに運び始めたときの 1 回の計測である。運んでいる間の数字は文字の量でほとんど変わらない。内部モードで測ったもので、OS モードでは測っていない。

| 選択した量 | 開始（`update` と描画） | 運ぶ間の UI p50 | UI p99 | GPU p50 | 再描画（p50） |
|---|---|---|---|---|---|
| 131 KB（100 段落） | 1.62 ms | 0.49 | 0.93 | 0.62 | 30.3% |
| 6.6 MB（5000 段落） | 8.09 ms | 0.56 | 1.00 | 0.59 | 30.3% |

再描画が大きいのは、画像が窓いっぱいの大きさで動くためである。

テストは次のものがある。

- `anti-webui-core/tests/drag.rs`: 合成イベントでの動作（48 件）
- `anti-webui-core/tests/text_drag.rs`: 選択した文字のドラッグと入力欄へのドロップ（41 件）。`text/edit.rs` の単体テストに移動と挿入
- `anti-webui-widgets/tests/sortable.rs`、`anti-webui-md3e/tests/sortable.rs`: 並べ替え
- `anti-webui-app/tests/drag.rs`: 本物のレンダラでのピクセル。部分再描画と全体の描画が 1 ピクセルも違わないこと（拡大率 1.8）
- `anti-webui-wayland/tests/drag_protocol.rs`: `wayland-server` で作った模擬コンポジタに対する要求の順序と内容。プロトコルに沿っていることは分かるが、本物のコンポジタの動きは分からない

`Gpu::headless` は Instance と Adapter をプロセスで 1 つだけ作るようにした。テストが並行して Instance を作って壊すと、Vulkan ローダーの中で落ちることがあったためである（スタックにこのプロジェクトのコードはなかった。ローダー側の原因までは調べていない）。アプリの窓の経路は変えていない。

## 確認していないこと

- **自動テストでの実機のポインタ操作。** ここの環境にはポインタを合成する手段がない。実機での操作は手で試して問題なさそうだった（2026-11-05）が、項目ごとの記録はない。
- **選択した文字のドラッグの実機での動作。** 他のアプリとの受け渡し、アイコンの見た目、自分の窓の中でのドロップは、模擬イベントと描画のテストまでで、手ではまだ試していない。
- Chrome の DnD とのメモリや遅延の比較、dGPU と 60 Hz での数字。

## 今回やらないこと

- タブの切り離し（`xdg-toplevel-drag`。複数ウィンドウの設計が先）
- タッチ（`wl_touch` がまだない）
- 単一行の入力欄の端での横の自動スクロール
- 取り消したときにプレビューが元の場所へ戻るアニメーション
- `Sheet` の `drag_over`（必要になった部品がない）
- 大きなデータを要求されてから作ること（`DragData` は動き出す時点で全部持つ）
