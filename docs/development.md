# 開発者向けガイド

[使い方に戻る](../README.md)

## ビルド環境

Python 3.11以降を使用する。 [GPUI Kitの要件](https://gpui-kit.com/docs/installation/)

| 環境 | Rust | 追加のビルド要件 |
| --- | --- | --- |
| Windows x64 | 1.95.0 | Visual Studio 2022のC++デスクトップ開発、Windows SDK、PATH上のCMake |
| macOS | 1.98.1 | Xcode Command Line Tools |
| Ubuntu 24.04 x64 | 1.95.0 | 下記の開発用パッケージ |

macOSではシェルで次を実行する。

```sh
rustup toolchain install 1.98.1 --profile minimal --component rustfmt
export RUSTUP_TOOLCHAIN=1.98.1
```

Ubuntu 24.04ではビルド依存をインストールする。

```sh
sudo apt-get update
sudo apt-get install -y gcc g++ clang libfontconfig-dev libwayland-dev \
  libwebkit2gtk-4.1-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libssl-dev libzstd-dev vulkan-validationlayers libvulkan1
```

## 起動とテスト

リポジトリのルートで実行する。環境に応じて `python` を `python3` に読み替える。

```sh
python scripts/package.py prepare
cargo run --locked
cargo test --locked
```

`prepare` は固定のSHA-256を検証したPDFium 8125を、実行ファイルとテスト実行ファイルの隣に配置する。`ELAFRA_REQUIRE_PDFIUM=1` を設定すると、PDFiumの欠落もテストで失敗として扱う。 [PDFium 8125](https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F8125)

## パッケージとリリース

```sh
python scripts/package.py package
```

実行した環境向けのパッケージが `dist/` に生成される。Windowsでインストーラーも作る場合はInno Setup 6を用意し、`python scripts/package.py package --installer` を実行する。

GitHub ActionsはWindows x64、macOS Apple Silicon、macOS Intel、Linux x64をテスト・ビルドする。`Cargo.toml` のバージョンと一致するタグ（例 `v0.1.0`）をプッシュすると、4構成の成功後に配布物をGitHub Releasesへ公開する。 [GitHub Actions](https://github.com/Ki2Neon/elafra/actions) · [GitHub Releases](https://github.com/Ki2Neon/elafra/releases)

## 描画とメモリ

- スライド一覧はサムネイル、現在スライドは高解像度画像を、物理メモリの空きに応じて先読みする。現在スライドと観客用は、接続中で画素数が最大のモニターに収まる画像を共有する。PDFページの縦横比を維持する。
- 表示中のページはキャッシュを使って表示し、追加描画が可能なら実際の表示領域の物理ピクセル寸法に合わせた画像へ切り替える。連続操作では手元の選択を先に更新し、観客画面に最後のページを反映する。
- 観客用キャッシュ容量に全ページが収まらなければ、現在ページとその前後を優先する。容量を変更すると対象を組み直す。表示中のページが容量を超える場合も画面には表示する。
- 物理メモリ使用率80%を基準に新規描画を制限する。資料の差し替えで空きが足りないときは、表示中のページを残して旧資料の先読み画像を解放し、再試行する。

## 画面で確認する項目

- **PDFを開く** — `tests/fixtures/basic.pdf` をファイルメニュー、現在スライドのボタン、ドロップ、履歴から開く。日本語名、壊れたPDFも確認する。`tests/fixtures/encrypted.pdf` のテスト用パスワードは `elafra`。誤入力と取消を含めて確認する。
- **投影とページ送り** — 画面が1台の場合と複数の場合を試す。投影先変更、解像度の異なるモニターの接続・切断、全画面、連続ページ送り、PDF差し替え成功時の投影終了を確認する。LinuxはX11とXWaylandで確認する。
- **カウンター** — 自動開始、手動操作、カウントダウンの満了、同じ資料での投影再開を確認する。
- **配置と保存** — 各パネルの分割、タブ化、極端なサイズ変更、閉じたパネルの再表示を確認する。一覧は縦・横・グリッドそれぞれで先頭・末尾・単一ページの中央表示を試す。再起動後の配置、テーマ、履歴を確認する。
- **描画とキャッシュ** — 縦長ページや細字のPDFでDPI倍率とウィンドウのサイズを変更する。少容量から `0 MB` への変更と再設定を繰り返し、操作と投影を確認する。
