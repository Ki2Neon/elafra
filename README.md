# Elafra

PDFを手元で操作し、別ウィンドウへ投影するデスクトップアプリ。Rust、GPUI、PDFium を使用する。

## 操作

1. `ファイル → PDFを読み込む`、PDF未読込時の `現在スライド → ファイルを開く`、ファイルのドラッグ＆ドロップ、または `最近開いたPDF` から資料を開く。選択ダイアログは `Ctrl+O`、macOS では `⌘+O` でも開く。
2. プレゼンター画面で開始ページを選び、ツールバーの `プレゼン開始` または `F5` で観客用画面を開く。投影先は `表示` メニューから変更できる。自動配置を確認できなかった場合は観客用ウィンドウを移動し、`F11` で全画面表示する。
3. 一覧クリック、左右矢印、`Page Up`／`Page Down` でページを移動する。連続操作ではプレゼンター側の選択を先に更新し、観客用画面は操作が落ち着いた時点のページを表示する。投影中に初めてページが変わると計測を開始する。
4. `Esc` またはツールバーの `プレゼン終了` で終了する。同じ資料を再投影した場合は、最初のページ移動で前回の計測値から再開する。

ドックのタブをドラッグすると分割・タブ化できる。境界をドラッグしてサイズを調整し、閉じたドックは `表示` メニューから呼び出す。各ドックの右クリックメニューで設定を変更できる。

スライド一覧の右クリックメニューで `縦並び`、`横並び`、`グリッド` を切り替える。縦・横並びでは現在ページ、グリッドでは現在ページの行がスクロール方向の中央に移動する。横並びはマウスホイールでも左右に移動できる。サムネイルはページ全体と1から始まる番号を表示し、番号だけの表示にも切り替えられる。

## 設定と先読み

ツールバーの `設定` からライト／ダークテーマと観客用キャッシュ容量を変更する。初期値はライト・128 MB（128 MiB）。`0 MB` で観客用の先読みが無効になる。

- スライド一覧は全ページのサムネイル、現在スライドは全ページの高解像度画像を物理メモリの空きに応じて先読みする。現在スライドと観客用は、接続中で画素数が最大の画面に合わせて用意した画像を共有する。
- 現在ページはキャッシュから即時に表示し、実際の表示領域の物理ピクセル寸法でPDFiumが描画した画像へ切り替える。追加描画に必要なメモリが足りない場合はキャッシュ画像で表示を続ける。
- 観客用キャッシュは、設定容量に収まれば全ページを保持し、収まらなければ現在ページとその前後を優先する。容量を変更すると対象ページを再計算し、先読みを再開する。表示中の1ページが設定容量を超えるときは、観客用キャッシュとは別枠で表示する。
- システムの物理メモリ使用率80%を基準に新規描画を制限する。資料の差し替え時に空きが足りなければ、表示中のページを残して旧資料の先読み画像を解放し、再試行する。資料や移動先を表示できない場合は、直前の表示を維持し、ツールバー内に一時的に `メモリ不足` と表示する。

## 対応環境

| OS | 配布形式 | 実行環境 |
| --- | --- | --- |
| Windows x64 | インストーラー EXE、ポータブル ZIP | Windows 10 22H2 または Windows 11 |
| macOS Apple Silicon／Intel | ZIP | macOS 15以降。CPUに対応する ZIP を展開し、`Elafra.app` を起動 |
| Linux x64 | tar.gz | Ubuntu 24.04。展開先の `elafra` を起動 |

Windows の実行には Visual C++ v14 Redistributable (x64) が必要。 [Microsoft のダウンロードページ](https://learn.microsoft.com/ja-jp/cpp/windows/latest-supported-vc-redist)

Windows の `-setup.exe` はユーザー単位でインストールし、スタートメニューに Elafra を追加する。ポータブル版は書き込み可能なフォルダーに展開して `elafra.exe` を起動する。ポータブル版の設定と履歴は展開先の `settings.json` に保存する。

Linux は X11 または XWayland と Vulkan 対応ドライバーを使用する。Ubuntu で実行時ライブラリが入っていない場合は `sudo apt install libxcb1 libxkbcommon0 libxkbcommon-x11-0` で導入する。ファイル選択にはデスクトップ環境に対応する XDG Desktop Portal を使う。Ubuntu GNOME の場合は `xdg-desktop-portal-gnome`、ファイル選択の代替には `zenity` をインストールする。

macOS のバンドルは起動用のアドホック署名を付けて配布する。ダウンロードしたバンドルの初回起動時、Gatekeeper が開発元の確認を求める場合がある。

## ビルドと配布

Rust 1.95.0 と Python 3.11 以降を使用する。Windows は MSVC ツールチェーン、Visual Studio 2022 の C++ デスクトップ開発と Windows SDK、PATH 上の CMake が必要。macOS は Xcode Command Line Tools と Rust 1.98.1 が必要。macOS では `rustup toolchain install 1.98.1 --component rustfmt` で追加し、ビルドやテストに `RUSTUP_TOOLCHAIN=1.98.1` を指定する。 [GPUI Kit の要件](https://gpui-kit.com/docs/installation/)

Ubuntu 24.04 のビルド依存をインストールする。

```sh
sudo apt-get update
sudo apt-get install -y gcc g++ clang libfontconfig-dev libwayland-dev \
  libwebkit2gtk-4.1-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libssl-dev libzstd-dev vulkan-validationlayers libvulkan1
```

ローカルでの起動とテスト。

```sh
python scripts/package.py prepare
cargo run --locked
cargo test --locked
```

環境によっては `python` を `python3` に読み替える。`ELAFRA_REQUIRE_PDFIUM=1` を設定してテストすると、PDFium が見つからない場合もエラーとして検出する。

```sh
python scripts/package.py package
```

ネイティブのリリースビルドを `dist/` に生成する。スクリプトは PDFium `8125` の配布元のアーカイブを取得し、固定の SHA-256 を検証してライブラリとライセンスを同梱する。CI は4ターゲットでテストと配布物生成を行い、成果物を GitHub Actions に添付する。 [PDFium 8125](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F8125)

Windows でインストーラーも生成するには Inno Setup 6 を用意し、`python scripts/package.py package --installer` を実行する。

`Cargo.toml` と一致するバージョンタグ（例 `v0.1.0`）をプッシュすると、GitHub Actions がテストとビルドを実行し、成功した配布物を GitHub Releases に公開する。 [Elafra Releases](https://github.com/Ki2Neon/elafra/releases)

## GUI 検証

1. 起動直後はプレゼンター画面のみ表示されることを確認する。`tests/fixtures/basic.pdf` をファイルメニュー、現在スライドの `ファイルを開く`、ドロップ、履歴から読み込み、縦横比の異なる赤・青のページを確認する。日本語名へコピーして再度開く。
2. `tests/fixtures/encrypted.pdf` を開く。誤ったパスワード、取消、正しい `elafra` を試す。投影中は読込成功時にだけ投影が終了することを確認する。破損PDFのエラーも確認する。
3. 1画面ではウィンドウ、2画面では別モニターの全画面になることを確認する。投影先を変更し、ページとタイマーの値を確認する。解像度の異なるモニターの接続・切断後もページが表示されることを確認する。Linux は X11 と XWayland で確認する。
4. 一覧とキーでページを送り、先頭・末尾、短時間の連続操作、投影終了後の再開を試す。カウンターの初回移動、手動操作、同一資料の再投影、カウントダウンの満了を確認する。
5. ドックの幅や高さを狭め、計測操作、一覧の選択とスクロール、未読込時のファイル選択を確認する。分割・タブ化・非表示、`表示` メニューからの呼び出し、右クリック設定も試す。
6. `設定` を開き、テーマと観客用キャッシュ容量の保存・取消を試す。再起動後に設定、ドック配置、履歴が復元されることを確認する。少容量または `0 MB` でも資料を操作し、投影できることを確認する。
7. 一覧で縦並び、横並び、グリッドを切り替える。先頭・末尾・単一ページでの中央寄せ、手動スクロール、ドックのリサイズ、再起動後の配置復元を確認する。
8. 細字・細線を含むPDFを観客用と現在スライドに表示する。DPI倍率、ウィンドウの大きさ、表示ページを変えて、ページの全体表示と鮮明さを確認する。
