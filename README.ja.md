# TetherMoon 🌙

![TetherMoon](gallery/sns-wide.png)

*[English README](README.md) · [한국어 README](README.ko.md)*

**Sony Camera Remote SDK** の Rust FFI ラッパー＋ブラウザベースの **テザリングサーバー**
（シングルページ Web UI）。スマホや PC のブラウザから、露出・フォーカス・撮影・ライブビュー・
長秒露光／タイムラプスをリモート操作します。

> ### ⚠️ 対象機種：Sony A7C（ILCE-7C）専用
> **ILCE-7C 1 台のみ**で、macOS（Apple Silicon）・USB 環境で開発・検証しています。他のボディは
> 未検証です。A7C が公開しない機能（ジャイロ水準器・クリエイティブルック・バルブタイマー・
> AF エリア device property など）はコードに残っていても、この機種では動作しません。マルチ
> ボディ対応は今後の課題です。

## クイックスタート — とにかく使いたい方へ

ビルド不要です。最新の **[リリース](../../releases/latest)** をどうぞ：

1. `.dmg` をダウンロードして開き、**TetherMoon** を Applications にドラッグ。
2. 初回のみ、隔離属性を解除（アプリは adhoc 署名で公証なし）：ターミナルで
   `xattr -dr com.apple.quarantine /Applications/TetherMoon.app` を実行してから
   アプリを開きます。（macOS 15 Sequoia 以降は「右クリック → 開く」の回避が
   使えなくなったため、このコマンドで「壊れている」エラーを防ぎます。）
3. A7C を USB で接続し、**PC リモート** に設定（カメラ：MENU → USB →
   *USB接続モード* → *PCリモート*）。コンソールがブラウザで自動的に開きます。
4. スマホで見るには、ページ下部に表示される LAN URL をスマホのブラウザで開きます
   （スマホは同じ Wi-Fi に接続）。

これ以降は **ソースからビルド** する方向けの内容です。

## 機能

- **ライブビュー** — MJPEG ストリーム＋フォーカスピーキング、三分割グリッド（オン/オフ・
  ビューと一緒に回転）、RGB ヒストグラム、手動回転
- **露出・色** — ISO・シャッター・絞り・EV・ホワイトバランス（＋色温度スライダー）・
  測光・ドライブ・フラッシュモード・ファイル形式・JPEG 画質・ピクチャープロファイル
- **フォーカス** — MF Near/Far スライダー、ライブビュー・タップで AF ポイント指定（Y 軸補正・
  回転対応）、AF エリアモード（ワイド/ゾーン/中央/フレキシブル S・M・L/トラッキング）、
  半押し（S1）＋合焦表示
- **ソフトウェア AF** — MF 時のコントラスト検出 AF：ライブビューで点を選ぶと、サーバーが
  多重解像度フォーカススイープ（coarse 範囲 → fine ズームイン）でその ROI のラプラシアン分散
  シャープネスを測定。バックラッシュに強い着地＋リアルタイムの ROI ボックス・進捗。A7C のように
  絶対フォーカス API を持たないボディでも動作
- **トラッキング AF** *(実験的・macOS・オプション)* — オプションの検出モジュール
  （`--features detector`）がライブビューで **RT-DETR** 物体検出を **CoreML**（Apple
  Neural Engine、ほぼリアルタイム）で実行。オーバーレイの検出ボックスを選ぶとその物体を追跡し、
  重心をソフトウェア AF のフォーカス ROI に供給。デフォルトビルドには含まれない（ML 依存なし）
- **撮影** — 1 枚・連写（押し続け）・動画記録・キャンセル
- **多重露光** — ソフトウェア多重露光（A7C 本体にはない機能）：N 枚撮影してサーバー側で 1 枚に
  合成 — *平均*（クラシック）・*比較明（lighten）*（明るい画素を蓄積 — 光跡・星）・*加算（add）*
- **ミニマルリモコン** — プレビューなしのモバイルページ（レリーズ感覚）：シャッター・AF・
  動画＋ ISO/シャッター/絞り、ライブビュー配信なし（バッテリーに優しい）
- **長秒露光** — 固定 1″〜30″、BULB、**ソフトウェア・バルブタイマー**（1〜900 秒）
- **タイムラプス** — ソフトウェア・インターバル撮影（枚数 × 間隔）＋キャンセル
- **保存** — PC 保存（フォルダ・ファイル名プレフィックス）、撮影プレビュー、直近サムネイル、
  バッテリー残量・残り撮影枚数
- **マルチボディ対応** — 接続したボディが報告する capability に合わせて UI を自動調整。
  非対応のプロパティは自動的に非表示
- **複数端末で同時視聴** — 1 台のカメラのストリームを任意の数のブラウザへ fan-out
  （スマホ＋PC 同時）
- **安定動作** — 自動再接続、クリーンな終了（カメラセッションを確実に解放）、起動時に
  ブラウザを自動で開く

### 天体撮影ツール (1.0 以降)

- **赤いナイトモード** — 🌙 で UI 全体を低輝度の赤に。暗所での目の暗順応を保つ（コンソール・
  ミニマルリモコン共有）
- **Star AF** — 画面内で最も明るい星/点にワンタップでオートフォーカス
- **ピントメーター** — 最も明るい星の実時間シャープネス。MF を回して数値のピークで最良ピント
  （マスク不要の Bahtinov 相当）
- **露出ブラケット (AEB)** — 露出補正リストを送りながら N 枚撮影
- **RAW プレビュー** — `.arw` は埋め込み JPEG を取り出してプレビュー
- **ライブスタック** *(Stack タブ)* — フレームを星で整列して積算、SNR 向上（Average）または明るい
  部分の積算（Lighten）。ライブビューからリアルタイム、または保存フレームのフル解像度ポスト
  スタック、**ラッキーイメージング**（シャープな上位 % のみ）、**整列 3 モード** — Stars（深宇宙）・
  Centroid（月・惑星）・ROI（ボックスをドラッグして特徴に固定）。本物の **RAW スタック**（rawler
  デモザイク + 16bit リニア積算）は `--features raw` ビルドで
- **撮影プラン** *(Plan タブ)* — 位置（または端末 GPS）を入れると、今どの対象がどれくらいの高度で
  出ているか。太陽・月・惑星 + 明るい星・深宇宙天体のカタログ、カスタム対象、「これを撮る」
  プリセット受け渡し

## スクリーンショット

シングルページの **Tether Console** — 左にフォーカスピーキングと三分割グリッド付きの
ライブビュー、右にすべてのコントロール。

| 接続中 | ライブビュー（MF ピント送り） |
|---|---|
| ![connected UI](gallery/ui-connected.png) | ![live view](gallery/ui-liveview.png) |

![full UI, disconnected](gallery/ui-disconnected.png)

## アーキテクチャ

```
Sony C++ SDK ──► wrapper/wrapper.{h,cpp}  (純粋 C シム、SCRSDK 名前空間ブリッジ)
                     └─► build.rs (cc + bindgen) ─► src/ffi.rs
                            └─► 安全な Rust lib: session / enumerate / connection /
                                liveview / shutter / control / properties / callback / error
                                   └─► crsdk_server (axum/tokio) + web/{index.html, styles.css, app.js}
```

すべての SDK 呼び出しは `spawn_blocking` で隔離し、カメラは `Arc<Mutex<…>>` の背後に置きます。

## ビルド

**Sony SDK は本リポジトリに同梱していません**（下記 *ライセンス* 参照）。各自でダウンロードし、
プロジェクトルートに `CrSDK_v2.01.00_20260203a_Mac/` として配置してください。

```bash
# 前提: Rust, LLVM/Clang (brew install llvm)
export DYLD_LIBRARY_PATH=$DYLD_LIBRARY_PATH:$(pwd)/CrSDK_v2.01.00_20260203a_Mac/RemoteCli/external/crsdk/

cargo run -p crsdk_server        # → http://localhost:8080/web/index.html
```

macOS の `ptpcamerad` デーモンが USB カメラへのアクセスを妨げるため、サーバーは起動時にこれを
抑制します（正常な動作です）。

## 配布（バイナリ .app）

Sony ライセンスは、SDK ライブラリを **アプリ内に同梱して** 配布することを許可しています。
`scripts/make_app.sh` は SDK ライブラリを `Contents/Frameworks/` に含む自己完結型の macOS
アプリバンドル（`dist/TetherMoon.app`）を作成します:

```bash
./scripts/make_app.sh
```

ビルド済みの配布物は [Releases](../../releases) にあります。初回起動：右クリック → 開く、
または `xattr -dr com.apple.quarantine "TetherMoon.app"`。

## 🌙 最初の一枚

このツールで撮影 — ILCE-7C + FE 100-400 GM、撮って出し。

![first moon](gallery/first-moon.jpg)

> © neko.kim.film (キム・ゲンフィルム)

## 支援

このプロジェクトが役に立ったら ☕

[![Support on Ctee](https://img.shields.io/badge/Ctee-sdlfactory-FF5A5F)](https://ctee.kr/place/sdlfactory)

## お問い合わせ

質問・バグ報告・フィードバック: **spacedlfactory@gmail.com**

## ライセンス

本リポジトリのソースコードは **MIT ライセンス**（[LICENSE](LICENSE)）です。

**Sony Camera Remote SDK は同梱されておらず**、**著作権は Sony** にあります。
[Sony Developer World](https://www.sony.net/CameraRemoteSDK/) からダウンロードし、
[ライセンス契約](https://support.d-imaging.sony.co.jp/app/sdk/licenseagreement/ja.html)に
同意する必要があります。本プロジェクトは Sony とは無関係の独立・非公式プロジェクトです。
