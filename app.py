import streamlit as st
import os
import time
from playwright.sync_api import sync_playwright
from dotenv import load_dotenv

# .env ファイルから環境変数を読み込む
load_dotenv()

# ----------------------------------------------------
# 画面の初期設定
# ----------------------------------------------------
st.set_page_config(page_title="YouTube一括アップローダー", layout="centered")
st.title("🎬 YouTube 一括アップローダー (uv + Playwright)")

# サンプル用の既存の再生リスト一覧
EXISTING_PLAYLISTS = ["お気に入り動画", "プログラミング講座", "ゲーム実況", "その他"]

# ----------------------------------------------------
# 環境変数からChromeのユーザーデータパスを取得
# ----------------------------------------------------
CHROME_USER_DATA_DIR = os.getenv("CHROME_USER_DATA_DIR")

if not CHROME_USER_DATA_DIR:
    st.error("❌ `.env` ファイルに `CHROME_USER_DATA_DIR` が設定されていません。`.env.example` を参考に作成してください。")
    st.stop()

# ----------------------------------------------------
# PlaywrightによるYouTube Studioの自動操作ロジック
# ----------------------------------------------------
def upload_to_youtube_playwright(file_path, title, desc, privacy, playlist_option, playlist_name):
    st.write(f"🤖 ブラウザを起動して YouTube Studio にアクセス中...")
    
    with sync_playwright() as p:
        try:
            # ログインセッションを引き継いで起動
            context = p.chromium.launch_persistent_context(
                user_data_dir=CHROME_USER_DATA_DIR,
                channel="chrome",
                headless=False,
                args=["--no-sandbox", "--disable-setuid-sandbox"]
            )
        except Exception as e:
            st.error("❌ Chromeを開けませんでした。本物のChromeブラウザが完全に終了しているか確認してください。")
            st.code(str(e))
            return False
        
        page = context.new_page()
        page.goto("https://youtube.com")
        time.sleep(3)
        
        if not page.locator("#create-icon").is_visible():
            st.error("❌ YouTube Studioにログインしていません。ブラウザ上でログインを完了させてから再試行してください。")
            context.close()
            return False

        st.write("🔼 アップロードを開始します...")
        page.click("#create-icon")
        page.click("#create-menu-item-0")
        time.sleep(1)
        
        file_input = page.locator('input[type="file"]')
        file_input.set_input_files(file_path)
        st.write("⏳ 動画ファイルの転送中...（動画の長さにより時間がかかります）")
        time.sleep(5)

        title_box = page.locator("#title-textarea #textbox")
        title_box.click()
        title_box.press("Meta+A" if os.name != "nt" else "Control+A")
        title_box.press("Backspace")
        title_box.fill(title)
        
        desc_box = page.locator("#description-textarea #textbox")
        desc_box.fill(desc)
        
        if playlist_option != "再生リストに追加しない":
            st.write(f"📂 再生リスト「{playlist_name}」への追加処理を実行中...")
            page.locator(".dropdown-trigger").click()
            time.sleep(1)
            
            if playlist_option == "既存のリストから選択":
                playlist_checkbox = page.locator(f'span:has-text("{playlist_name}")')
                if playlist_checkbox.is_visible():
                    playlist_checkbox.click()
                else:
                    st.warning(f"⚠️ 再生リスト '{playlist_name}' が見つかりませんでした。")
            
            elif playlist_option == "新規リストを作成して追加":
                page.locator("text=新しい再生リスト").click()
                page.locator("text=新しい再生リストを作成").click()
                page.locator('input[placeholder="タイトルを追加（必須）"]').fill(playlist_name)
                page.locator("text=作成").click()
                time.sleep(1)
            
            page.locator("text=完了").click()
            time.sleep(1)

        st.write("➡️ 設定項目を進行中...")
        for _ in range(3):
            page.click("#next-button")
            time.sleep(1.5)
            
        st.write(f"🔒 公開範囲を「{privacy}」に設定中...")
        if "限定公開" in privacy:
            page.locator('tp-yt-paper-radio-button[name="UNLISTED"]').click()
        elif "非公開" in privacy:
            page.locator('tp-yt-paper-radio-button[name="PRIVATE"]').click()
            
        time.sleep(1)
        
        st.write("💾 動画を保存しています...")
        page.click("#done-button")
        time.sleep(3)
        
        context.close()
    return True

# ----------------------------------------------------
# GUI フォーム画面
# ----------------------------------------------------
with st.form("upload_form", clear_on_submit=False):
    st.subheader("1. 動画ファイルの選択（複数可）")
    uploaded_files = st.file_uploader(
        "アップロードしたい動画ファイル（MP4等）を選択してください", 
        type=["mp4", "mov", "mkv"], 
        accept_multiple_files=True
    )

    st.subheader("2. 投稿設定（一括適用）")
    title_suffix = st.text_input("タイトル（ファイル名＋このテキストになります）", placeholder="（例：_バックアップ）")
    description = st.text_area("共通の説明文（概要欄）", placeholder="動画の説明をここに記入")

    privacy = st.selectbox("公開設定", ["限定公開 (Unlisted)", "非公開 (Private)", "公開 (Public)"])
    if privacy == "公開 (Public)":
        st.warning("⚠️ 注意: 現在「公開」が選択されています。個人ツールでの即時公開は推奨されません。")
        st.toast("公開設定が選択されています。ご注意ください！", icon="⚠️")

    st.markdown("**再生リストの設定**")
    playlist_option = st.radio("再生リストの追加方法", ["既存のリストから選択", "新規リストを作成して追加", "再生リストに追加しない"], index=2)

    selected_playlist = None
    if playlist_option == "既存のリストから選択":
        selected_playlist = st.selectbox("既存の再生リストを選択", EXISTING_PLAYLISTS)
    elif playlist_option == "新規リストを作成して追加":
        selected_playlist = st.text_input("新しい再生リスト名を入力してください")

    submit_button = st.form_submit_button("一括アップロードを開始")

# ----------------------------------------------------
# アップロードの実行処理
# ----------------------------------------------------
if submit_button:
    if privacy == "公開 (Public)":
        st.error("❌ セキュリティ上の安全のため、「公開 (Public)」状態での自動アップロードは現在ブロックされています。")
    elif not uploaded_files:
        st.error("❌ 動画ファイルが選択されていません。")
    elif playlist_option == "新規リストを作成して追加" and not selected_playlist.strip():
        st.error("❌ 新しい再生リスト名を入力してください。")
    else:
        st.success(f"🚀 {len(uploaded_files)} 本の動画の一括処理を開始します...")
        os.makedirs("temp_uploads", exist_ok=True)
        
        for idx, file in enumerate(uploaded_files):
            st.markdown(f"---")
            st.markdown(f"### 📦 [{idx+1}/{len(uploaded_files)}] ファイル名: `{file.name}`")
            
            temp_path = os.path.join("temp_uploads", file.name)
            with open(temp_path, "wb") as f:
                f.write(file.getbuffer())
            
            base_name, _ = os.path.splitext(file.name)
            final_title = f"{base_name}{title_suffix}"
            
            success = upload_to_youtube_playwright(
                file_path=os.path.abspath(temp_path),
                title=final_title,
                desc=description,
                privacy=privacy,
                playlist_option=playlist_option,
                playlist_name=selected_playlist
            )
            
            if success:
                st.success(f"✅ `{file.name}` のアップロードが完了しました。")
                if os.path.exists(temp_path):
                    os.remove(temp_path)
                    
        st.balloons()
        st.success("🎉 全ての動画の一括処理が完了しました！")
