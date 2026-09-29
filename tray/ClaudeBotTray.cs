using System;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.IO;
using System.Globalization;
using System.Net;
using System.Text.RegularExpressions;
using System.Windows.Forms;
using System.Threading;
using System.Runtime.InteropServices;

class ClaudeBotTray : Form
{
    [DllImport("user32.dll", CharSet = CharSet.Auto)]
    private static extern IntPtr SendMessage(IntPtr hWnd, int msg, IntPtr wParam, string lParam);
    private const int EM_SETCUEBANNER = 0x1501;

    [DllImport("dwmapi.dll")]
    private static extern int DwmSetWindowAttribute(IntPtr hwnd, int attr, ref int attrValue, int attrSize);
    private const int DWMWA_USE_IMMERSIVE_DARK_MODE = 20;

    private NotifyIcon trayIcon;
    private System.Windows.Forms.Timer refreshTimer;
    private System.Windows.Forms.Timer updateCheckTimer;
    private string botDir;
    private string envPath;
    private string taskName = "ClaudeDiscordBot";
    private string currentVersion = "unknown";
    private bool updateAvailable = false;
    private string cachedReleaseNotes = "";
    private string cachedNewVersion = "";
    private bool lastPanelRunning = false;
    private bool botStarting = false;
    private Color lastIconColor = Color.Empty;
    private IntPtr lastHIcon = IntPtr.Zero;
    private string lastStatusText = "";

    // Usage data
    private double usageFiveHour = -1;
    private double usageSevenDay = -1;
    private double usageSevenDaySonnet = -1;
    private string usageFiveHourReset = "";
    private string usageSevenDayReset = "";
    private string usageSevenDaySonnetReset = "";
    private DateTime? usageLastFetched = null;
    private System.Windows.Forms.Timer usageTimer;

    // Language support
    private string langPrefFile;
    private bool isKorean = false;

    public ClaudeBotTray()
    {
        botDir = Path.GetDirectoryName(Path.GetDirectoryName(Application.ExecutablePath));
        envPath = Path.Combine(botDir, ".env");
        langPrefFile = Path.Combine(botDir, ".tray-lang");

        // Load saved language preference
        try
        {
            if (File.Exists(langPrefFile))
            {
                string saved = File.ReadAllText(langPrefFile).Trim();
                isKorean = (saved == "kr");
            }
        }
        catch { }

        this.ShowInTaskbar = false;
        this.WindowState = FormWindowState.Minimized;
        this.FormBorderStyle = FormBorderStyle.None;
        this.Opacity = 0;

        currentVersion = GetVersion();

        trayIcon = new NotifyIcon();
        trayIcon.Visible = true;
        // Left-click opens control panel window
        trayIcon.MouseClick += (s, e) => {
            if (e.Button == MouseButtons.Left)
            {
                ShowControlPanel();
            }
        };
        UpdateStatus();
        BuildMenu();

        refreshTimer = new System.Windows.Forms.Timer();
        refreshTimer.Interval = 5000;
        refreshTimer.Tick += (s, e) => { try { if (UpdateStatus()) BuildMenu(); } catch { } };
        refreshTimer.Start();

        // Check for updates every 5 hours
        updateCheckTimer = new System.Windows.Forms.Timer();
        updateCheckTimer.Interval = 18000000;
        updateCheckTimer.Tick += (s, e) => { try { CheckForUpdates(); BuildMenu(); } catch { } };
        updateCheckTimer.Start();

        // Initial update check
        CheckForUpdates();

        // Load cached usage, then fetch fresh
        LoadUsageCache();

        // Usage fetch timer (every 5 minutes)
        usageTimer = new System.Windows.Forms.Timer();
        usageTimer.Interval = 300000;
        usageTimer.Tick += (s, e) => { try { if (controlPanel != null && !controlPanel.IsDisposed && controlPanel.Visible) FetchUsage(); } catch { } };
        usageTimer.Start();
        FetchUsage();

        bool showPanel = false;
        string[] args = Environment.GetCommandLineArgs();
        for (int i = 0; i < args.Length; i++)
        {
            if (args[i] == "--show") showPanel = true;
        }

        if (!IsEnvConfigured())
        {
            // .env 없거나 설정 안 됐으면 패널 열기 (패널에서 설정 버튼 제공)
            System.Windows.Forms.Timer t = new System.Windows.Forms.Timer();
            t.Interval = 500;
            t.Tick += (s, e) => { t.Stop(); ShowControlPanel(); };
            t.Start();
        }
        else if (!IsRunning())
        {
            // .env 있고 봇이 안 돌고 있으면 자동 시작
            System.Windows.Forms.Timer t = new System.Windows.Forms.Timer();
            t.Interval = 1000;
            t.Tick += (s, e) => { t.Stop(); StartBot(null, null); };
            t.Start();
        }

        if (showPanel)
        {
            System.Windows.Forms.Timer st = new System.Windows.Forms.Timer();
            st.Interval = 1500;
            st.Tick += (s, e) => { st.Stop(); ShowControlPanel(); };
            st.Start();
        }
    }

    // --- Localization ---
    private string L(string en, string kr)
    {
        return isKorean ? kr : en;
    }

    private void SetLanguage(bool korean)
    {
        isKorean = korean;
        try { File.WriteAllText(langPrefFile, korean ? "kr" : "en"); } catch { }
        UpdateStatus();
        BuildMenu();
    }

    private bool IsRunning()
    {
        // 1. Check ClaudeBot.exe process
        try
        {
            if (Process.GetProcessesByName("ClaudeBot").Length > 0) return true;
        }
        catch { }
        // 2. Check lock file (written by StartBot cmd chain)
        if (File.Exists(Path.Combine(botDir, ".bot.lock")))
        {
            // Verify lock file is not stale (older than 2 minutes without a matching process)
            try
            {
                var lockAge = DateTime.Now - File.GetLastWriteTime(Path.Combine(botDir, ".bot.lock"));
                if (lockAge.TotalMinutes < 2) return true;
                // Stale lock — check if any node/ClaudeBot process is actually running
                var proc = new Process();
                proc.StartInfo.FileName = "powershell";
                proc.StartInfo.Arguments = "-NoProfile -Command \"if (Get-WmiObject Win32_Process | Where-Object { $_.CommandLine -like '*dist/index.js*' }) { exit 0 } else { exit 1 }\"";
                proc.StartInfo.UseShellExecute = false;
                proc.StartInfo.CreateNoWindow = true;
                proc.Start();
                proc.WaitForExit(5000);
                if (proc.ExitCode == 0) return true;
                // Stale lock file with no process — clean up
                try { File.Delete(Path.Combine(botDir, ".bot.lock")); } catch { }
            }
            catch { return true; } // If we can't check, assume running
        }
        return false;
    }

    private string GetVersion()
    {
        try
        {
            return RunCmdOutput("git", "-C \"" + botDir + "\" describe --tags --always").Trim();
        }
        catch { return "unknown"; }
    }

    private void CheckForUpdates()
    {
        try
        {
            RunCmdOutput("git", "-C \"" + botDir + "\" fetch origin main --tags");
            string local = RunCmdOutput("git", "-C \"" + botDir + "\" rev-parse HEAD").Trim();
            string remote = RunCmdOutput("git", "-C \"" + botDir + "\" rev-parse origin/main").Trim();
            updateAvailable = !string.IsNullOrEmpty(local) && !string.IsNullOrEmpty(remote) && local != remote;
            if (updateAvailable) FetchReleaseNotes();
        }
        catch { updateAvailable = false; }
    }

    private string ExtractTag(string version)
    {
        var parts = version.Split('-');
        if (parts.Length >= 3 && parts[parts.Length - 1].StartsWith("g"))
        {
            var tagParts = new string[parts.Length - 2];
            Array.Copy(parts, tagParts, parts.Length - 2);
            return string.Join("-", tagParts);
        }
        return version;
    }

    private int[] ParseVersion(string tag)
    {
        string cleaned = tag.StartsWith("v") ? tag.Substring(1) : tag;
        var parts = cleaned.Split('.');
        var result = new int[parts.Length];
        for (int i = 0; i < parts.Length; i++)
        {
            int.TryParse(parts[i], out result[i]);
        }
        return result;
    }

    private bool IsNewerVersion(int[] a, int[] b)
    {
        int len = Math.Max(a.Length, b.Length);
        for (int i = 0; i < len; i++)
        {
            int av = i < a.Length ? a[i] : 0;
            int bv = i < b.Length ? b[i] : 0;
            if (av > bv) return true;
            if (av < bv) return false;
        }
        return false;
    }

    private void FetchReleaseNotes()
    {
        try
        {
            string currentTag = ExtractTag(currentVersion);
            int[] currentParts = ParseVersion(currentTag);
            // Use PowerShell to call GitHub API and format release notes
            string psScript =
                "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; " +
                "try { " +
                "$r = Invoke-RestMethod -Uri 'https://api.github.com/repos/chadingTV/claudecode-discord/releases' -TimeoutSec 10 -Headers @{'User-Agent'='claudecode-discord-tray'}; " +
                "$cv = '" + currentTag.Replace("'", "''") + "'.TrimStart('v').Split('.'); " +
                "$notes = @(); $latest = '" + currentTag.Replace("'", "''") + "'; " +
                "foreach($rel in $r) { " +
                "if($rel.draft) { continue }; " +
                "$rv = $rel.tag_name.TrimStart('v').Split('.'); " +
                "$newer = $false; " +
                "for($i=0; $i -lt [Math]::Max($rv.Length,$cv.Length); $i++) { " +
                "$a = if($i -lt $rv.Length){[int]$rv[$i]}else{0}; " +
                "$b = if($i -lt $cv.Length){[int]$cv[$i]}else{0}; " +
                "if($a -gt $b){$newer=$true;break}; if($a -lt $b){break} }; " +
                "if($newer) { " +
                "$body = $rel.body -replace '\\*\\*','' -replace '\\[([^\\]]+)\\]\\([^)]+\\)','$1'; " +
                "$body = ($body -split \\\"`n\\\" | Where-Object {$_ -notmatch 'Full Changelog:'}) -join \\\"`n\\\"; " +
                "$notes += \\\"--- $($rel.tag_name) ---`n$body\\\"; " +
                "$latest = $rel.tag_name } }; " +
                "Write-Output \\\"LATEST:$latest\\\"; " +
                "Write-Output 'NOTES:'; " +
                "$notes | ForEach-Object { Write-Output $_ } " +
                "} catch { Write-Output 'ERROR' }";
            string output = RunCmdOutput("powershell", "-NoProfile -ExecutionPolicy Bypass -Command \"" + psScript + "\"");
            string latestVersion = "";
            var noteLines = new System.Collections.Generic.List<string>();
            bool inNotes = false;
            foreach (var line in output.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries))
            {
                if (line.StartsWith("LATEST:")) latestVersion = line.Substring(7);
                else if (line == "NOTES:") inNotes = true;
                else if (inNotes) noteLines.Add(line);
            }
            cachedNewVersion = latestVersion;
            cachedReleaseNotes = string.Join("\r\n", noteLines.ToArray());
        }
        catch
        {
            cachedReleaseNotes = "";
            cachedNewVersion = "";
        }
    }

    private DialogResult ShowUpdateDialog()
    {
        var form = new Form()
        {
            Text = L("Update Available", "업데이트 가능"),
            Width = 500, Height = 450,
            StartPosition = FormStartPosition.CenterScreen,
            FormBorderStyle = FormBorderStyle.FixedDialog,
            MaximizeBox = false, MinimizeBox = false,
            BackColor = Color.FromArgb(30, 30, 30),
            ForeColor = Color.White,
        };
        int val = 1;
        DwmSetWindowAttribute(form.Handle, DWMWA_USE_IMMERSIVE_DARK_MODE, ref val, sizeof(int));

        DialogResult dialogResult = DialogResult.No;

        string versionText = string.IsNullOrEmpty(cachedNewVersion) ? "" : currentVersion + " → " + cachedNewVersion;
        var versionLabel = new Label()
        {
            Text = versionText, Left = 20, Top = 15, Width = 440, Height = 24,
            Font = new Font(FontFamily.GenericSansSerif, 12, FontStyle.Bold),
            ForeColor = Color.White, BackColor = Color.Transparent
        };
        form.Controls.Add(versionLabel);

        var descLabel = new Label()
        {
            Text = L("The bot will restart after updating.", "업데이트 후 봇이 재시작됩니다."),
            Left = 20, Top = 42, Width = 440, Height = 20,
            ForeColor = Color.Gray, BackColor = Color.Transparent,
            Font = new Font(FontFamily.GenericSansSerif, 9)
        };
        form.Controls.Add(descLabel);

        var notesBox = new TextBox()
        {
            Left = 20, Top = 70, Width = 440, Height = 280,
            Multiline = true, ReadOnly = true,
            ScrollBars = ScrollBars.Vertical,
            Text = cachedReleaseNotes,
            BackColor = Color.FromArgb(45, 45, 45), ForeColor = Color.White,
            Font = new Font("Consolas", 9),
            BorderStyle = BorderStyle.None,
        };
        form.Controls.Add(notesBox);

        var updateBtn = new Button()
        {
            Text = L("Update", "업데이트"), Left = 20, Top = 365, Width = 215, Height = 36,
            FlatStyle = FlatStyle.Flat, BackColor = Color.FromArgb(88, 101, 242), ForeColor = Color.White,
            Font = new Font(FontFamily.GenericSansSerif, 10, FontStyle.Bold)
        };
        updateBtn.FlatAppearance.BorderSize = 0;
        updateBtn.Click += (s, ev) => { dialogResult = DialogResult.Yes; form.Close(); };
        form.Controls.Add(updateBtn);

        var cancelBtn = new Button()
        {
            Text = L("Cancel", "취소"), Left = 245, Top = 365, Width = 215, Height = 36,
            FlatStyle = FlatStyle.Flat, BackColor = Color.FromArgb(55, 55, 55), ForeColor = Color.White,
            Font = new Font(FontFamily.GenericSansSerif, 10)
        };
        cancelBtn.FlatAppearance.BorderSize = 0;
        cancelBtn.Click += (s, ev) => { form.Close(); };
        form.Controls.Add(cancelBtn);

        form.ShowDialog();
        form.Dispose();
        return dialogResult;
    }

    private void PerformUpdate(object sender, EventArgs e)
    {
        DialogResult result;
        if (!string.IsNullOrEmpty(cachedReleaseNotes))
        {
            result = ShowUpdateDialog();
        }
        else
        {
            result = MessageBox.Show(
                L("Do you want to update to the latest version? The bot will restart after updating.",
                  "최신 버전으로 업데이트하시겠습니까? 업데이트 후 봇이 재시작됩니다."),
                L("Update Available", "업데이트 가능"),
                MessageBoxButtons.YesNo,
                MessageBoxIcon.Question);
        }

        if (result != DialogResult.Yes) return;

        bool wasRunning = IsRunning();
        if (wasRunning)
        {
            KillBot();
            Thread.Sleep(2000);
        }

        // Fetch and hard reset to avoid merge conflicts
        RunCmdOutput("git", "-C \"" + botDir + "\" fetch origin main --tags");
        string pullOutput = RunCmdOutput("git", "-C \"" + botDir + "\" reset --hard origin/main");

        // Check if pull succeeded by comparing versions
        string afterPull = RunCmdOutput("git", "-C \"" + botDir + "\" rev-parse HEAD").Trim();
        string remote = RunCmdOutput("git", "-C \"" + botDir + "\" rev-parse origin/main").Trim();
        if (!string.IsNullOrEmpty(afterPull) && !string.IsNullOrEmpty(remote) && afterPull != remote)
        {
            MessageBox.Show(
                L("Update failed. git pull may have failed:\n", "업데이트 실패. git pull이 실패했을 수 있습니다:\n") + pullOutput,
                L("Update Failed", "업데이트 실패"),
                MessageBoxButtons.OK, MessageBoxIcon.Error);
            if (wasRunning) StartBot(null, null);
            return;
        }

        // npm install, rebuild native modules & build
        RunCmd("cd /d \"" + botDir + "\" && npm install && npm rebuild better-sqlite3 && npm run build", true);

        currentVersion = GetVersion();
        updateAvailable = false;

        // Tray exe 재컴파일 및 재시작
        // 실행 중인 자기 자신은 삭제 불가하므로 bat 스크립트로 대기 후 교체
        string trayExe = Application.ExecutablePath;
        string traySrc = Path.Combine(Path.GetDirectoryName(trayExe), "ClaudeBotTray.cs");
        string updateBat = Path.Combine(botDir, ".tray-update.bat");

        if (File.Exists(traySrc))
        {
            string updateLog = Path.Combine(botDir, "update.log");
            string batContent =
                "@echo off\r\n" +
                "setlocal enabledelayedexpansion\r\n" +
                "set \"LOG=" + updateLog + "\"\r\n" +
                "echo [%date% %time%] Update started > \"%LOG%\"\r\n" +
                "taskkill /f /im ClaudeBotTray.exe >nul 2>&1\r\n" +
                "timeout /t 3 /nobreak >nul\r\n" +
                "del \"" + trayExe + "\" >nul 2>&1\r\n" +
                "set \"CSC=\"\r\n" +
                "for /f \"delims=\" %%i in ('dir /b /s \"%WINDIR%\\Microsoft.NET\\Framework64\\csc.exe\" 2^>nul') do set \"CSC=%%i\"\r\n" +
                "if \"!CSC!\"==\"\" (\r\n" +
                "    for /f \"delims=\" %%i in ('dir /b /s \"%WINDIR%\\Microsoft.NET\\Framework\\csc.exe\" 2^>nul') do set \"CSC=%%i\"\r\n" +
                ")\r\n" +
                "if \"!CSC!\"==\"\" (\r\n" +
                "    echo [%date% %time%] ERROR: csc.exe not found >> \"%LOG%\"\r\n" +
                "    goto :done\r\n" +
                ")\r\n" +
                "echo [%date% %time%] Compiling with !CSC! >> \"%LOG%\"\r\n" +
                "\"!CSC!\" /nologo /target:winexe /out:\"" + trayExe + "\" /reference:System.Windows.Forms.dll /reference:System.Drawing.dll \"" + traySrc + "\" >> \"%LOG%\" 2>&1\r\n" +
                "if exist \"" + trayExe + "\" (\r\n" +
                "    echo [%date% %time%] Compile OK, restarting >> \"%LOG%\"\r\n" +
                "    start \"\" \"" + trayExe + "\" --show\r\n" +
                ") else (\r\n" +
                "    echo [%date% %time%] ERROR: Compile failed >> \"%LOG%\"\r\n" +
                ")\r\n" +
                ":done\r\n";

            File.WriteAllText(updateBat, batContent);

            // Run update bat as independent process (survives parent exit)
            var psi = new ProcessStartInfo("cmd.exe", "/c \"" + updateBat + "\"")
            {
                WindowStyle = ProcessWindowStyle.Hidden,
                UseShellExecute = true,
            };
            Process.Start(psi);

            // 자기 자신 종료 (bat이 대기 후 처리)
            trayIcon.Visible = false;
            Application.Exit();
            return;
        }

        // traySrc 없으면 (비정상 상황) 그냥 봇만 재시작
        if (wasRunning)
        {
            StartBot(null, null);
        }

        MessageBox.Show(L("Updated to version: ", "업데이트 완료: ") + currentVersion,
            L("Update Complete", "업데이트 완료"), MessageBoxButtons.OK, MessageBoxIcon.Information);
        UpdateStatus();
        BuildMenu();
    }

    private string RunCmdOutput(string fileName, string arguments)
    {
        try
        {
            var proc = new Process();
            proc.StartInfo.FileName = fileName;
            proc.StartInfo.Arguments = arguments;
            proc.StartInfo.UseShellExecute = false;
            proc.StartInfo.RedirectStandardOutput = true;
            proc.StartInfo.CreateNoWindow = true;
            proc.StartInfo.WorkingDirectory = botDir;
            proc.Start();
            string output = proc.StandardOutput.ReadToEnd();
            proc.WaitForExit();
            return output;
        }
        catch { return ""; }
    }

    private Bitmap CreateIcon(Color color)
    {
        var bmp = new Bitmap(16, 16);
        using (var g = Graphics.FromImage(bmp))
        using (var brush = new SolidBrush(color))
        {
            g.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
            g.FillEllipse(brush, 1, 1, 14, 14);
        }
        return bmp;
    }

    private bool IsEnvConfigured()
    {
        if (!File.Exists(envPath)) return false;
        var env = LoadEnv();
        string token = ""; env.TryGetValue("DISCORD_BOT_TOKEN", out token);
        if (token == null || token == "" || token == "your_bot_token_here") return false;
        string guild = ""; env.TryGetValue("DISCORD_GUILD_ID", out guild);
        if (guild == null || guild == "" || guild == "your_server_id_here") return false;
        return true;
    }

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern bool DestroyIcon(IntPtr handle);

    private void SetTrayIcon(Color color)
    {
        // Skip if color hasn't changed — avoid GDI+ handle accumulation
        if (color == lastIconColor) return;
        lastIconColor = color;

        var bmp = CreateIcon(color);
        IntPtr newHIcon = bmp.GetHicon();
        bmp.Dispose();

        // Destroy previous native icon handle to prevent GDI+ leak
        IntPtr oldHIcon = lastHIcon;
        var oldIcon = trayIcon.Icon;

        trayIcon.Icon = Icon.FromHandle(newHIcon);
        lastHIcon = newHIcon;

        // Dispose managed Icon wrapper, then destroy old native handle
        if (oldIcon != null) { try { oldIcon.Dispose(); } catch { } }
        if (oldHIcon != IntPtr.Zero) { try { DestroyIcon(oldHIcon); } catch { } }
    }

    /// <summary>
    /// Updates tray icon and tooltip. Returns true if status actually changed.
    /// </summary>
    private bool UpdateStatus()
    {
        bool running = IsRunning();
        bool hasEnv = IsEnvConfigured();

        Color color;
        string text;
        if (!hasEnv)
        {
            color = Color.Orange;
            text = L("Claude Discord Bot: Setup Required", "Claude Discord Bot: 설정 필요");
        }
        else if (running)
        {
            color = Color.LimeGreen;
            text = L("Claude Discord Bot: Running", "Claude Discord Bot: 실행 중");
        }
        else
        {
            color = Color.Red;
            text = L("Claude Discord Bot: Stopped", "Claude Discord Bot: 중지됨");
        }

        bool changed = (text != lastStatusText);
        SetTrayIcon(color);
        trayIcon.Text = text;
        lastStatusText = text;
        return changed;
    }

    private void BuildMenu()
    {
        bool running = IsRunning();
        bool hasEnv = IsEnvConfigured();

        var oldMenu = trayIcon.ContextMenuStrip;
        var menu = new ContextMenuStrip();

        if (!hasEnv)
        {
            var noEnv = new ToolStripMenuItem(L("Setup Required", "설정 필요")) { Enabled = false };
            menu.Items.Add(noEnv);
            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add(L("Setup...", "설정..."), null, OpenSettings);
        }
        else
        {
            var status = new ToolStripMenuItem(running ? L("Running", "실행 중") : L("Stopped", "중지됨")) { Enabled = false };
            menu.Items.Add(status);
            menu.Items.Add(new ToolStripSeparator());

            if (running)
            {
                menu.Items.Add(L("Stop Bot", "봇 중지"), null, StopBot);
                menu.Items.Add(L("Restart Bot", "봇 재시작"), null, RestartBot);
            }
            else
            {
                menu.Items.Add(L("Start Bot", "봇 시작"), null, StartBot);
            }

            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add(L("Settings...", "설정..."), null, OpenSettings);
            menu.Items.Add(L("View Log", "로그 보기"), null, OpenLog);
            menu.Items.Add(L("Open Folder", "폴더 열기"), null, OpenFolder);
        }

        menu.Items.Add(new ToolStripSeparator());

        // Auto-start toggle
        var autoStartItem = new ToolStripMenuItem(L("Auto Run on Startup", "시작 시 자동 실행"));
        autoStartItem.Checked = IsAutoStartEnabled();
        autoStartItem.Click += ToggleAutoStart;
        menu.Items.Add(autoStartItem);

        // Language toggle in menu
        var langItem = new ToolStripMenuItem(isKorean ? "Language: KR" : "Language: EN");
        var enItem = new ToolStripMenuItem("English") { Checked = !isKorean };
        enItem.Click += (s, ev) => { SetLanguage(false); };
        var krItem = new ToolStripMenuItem("한국어") { Checked = isKorean };
        krItem.Click += (s, ev) => { SetLanguage(true); };
        langItem.DropDownItems.Add(enItem);
        langItem.DropDownItems.Add(krItem);
        menu.Items.Add(langItem);

        var versionItem = new ToolStripMenuItem(L("Version: ", "버전: ") + currentVersion) { Enabled = false };
        menu.Items.Add(versionItem);

        if (updateAvailable)
        {
            menu.Items.Add(L("Update Available", "업데이트 가능"), null, PerformUpdate);
        }
        else
        {
            menu.Items.Add(L("Check for Updates", "업데이트 확인"), null, (s, ev) => {
                CheckForUpdates();
                if (updateAvailable)
                {
                    BuildMenu();
                }
                else
                {
                    MessageBox.Show(
                        L("You are running the latest version.", "최신 버전을 사용 중입니다."),
                        L("No Updates", "업데이트 없음"),
                        MessageBoxButtons.OK, MessageBoxIcon.Information);
                }
            });
        }

        menu.Items.Add(new ToolStripSeparator());
        menu.Items.Add(L("Quit", "종료"), null, QuitAll);

        trayIcon.ContextMenuStrip = menu;
        if (oldMenu != null)
        {
            try
            {
                foreach (ToolStripItem item in oldMenu.Items) { try { item.Dispose(); } catch { } }
                oldMenu.Dispose();
            }
            catch { }
        }
    }

    private void AutoRebuildIfNeeded()
    {
        string distPath = Path.Combine(botDir, "dist", "index.js");
        string srcDir = Path.Combine(botDir, "src");
        bool needsRebuild = !File.Exists(distPath);
        if (!needsRebuild && Directory.Exists(srcDir))
        {
            DateTime distTime = File.GetLastWriteTime(distPath);
            foreach (string ts in Directory.GetFiles(srcDir, "*.ts", SearchOption.AllDirectories))
            {
                if (File.GetLastWriteTime(ts) > distTime) { needsRebuild = true; break; }
            }
        }
        if (needsRebuild)
        {
            RunCmd("cd /d \"" + botDir + "\" && npm install && npm run build", true);
        }
    }

    private void StartBot(object sender, EventArgs e)
    {
        botStarting = true;
        RebuildControlPanel();
        AutoRebuildIfNeeded();
        KillBot();
        // Copy node.exe as ClaudeBot.exe so it shows as "ClaudeBot" in Task Manager
        string claudeBotExe = Path.Combine(botDir, "ClaudeBot.exe");
        try
        {
            string whereOut = RunCmdOutput("where", "node").Trim();
            string nodeExe = whereOut.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries)[0].Trim();
            if (File.Exists(nodeExe))
            {
                if (!File.Exists(claudeBotExe) ||
                    File.GetLastWriteTime(nodeExe) > File.GetLastWriteTime(claudeBotExe))
                {
                    File.Copy(nodeExe, claudeBotExe, true);
                }
            }
        }
        catch { }
        // Use absolute path to ClaudeBot.exe so cmd can find it
        string botExePath = File.Exists(claudeBotExe) ? "\"" + claudeBotExe + "\"" : "node";
        // Run bot hidden via vbs
        string vbs = Path.Combine(botDir, ".bot-start.vbs");
        string cmd = "cmd /c cd /d " + botDir + " & echo running> .bot.lock & " + botExePath + " dist/index.js >> bot.log 2>&1 & del .bot.lock";
        File.WriteAllText(vbs, "Set ws = CreateObject(\"WScript.Shell\")\nws.Run \"" + cmd.Replace("\"", "\"\"") + "\", 0, False\n");
        Process.Start("wscript", "\"" + vbs + "\"");
        // Wait for bot to start, then show notification
        System.Windows.Forms.Timer waitTimer = new System.Windows.Forms.Timer();
        waitTimer.Interval = 1000;
        int waitCount = 0;
        waitTimer.Tick += (s2, e2) => {
            waitCount++;
            bool nowRunning = IsRunning();
            if (nowRunning)
            {
                waitTimer.Stop();
                botStarting = false;
                try { File.Delete(vbs); } catch { }
                UpdateStatus();
                BuildMenu();
                RebuildControlPanel();
                trayIcon.BalloonTipTitle = L("Claude Discord Bot Started", "Claude Discord Bot 시작됨");
                trayIcon.BalloonTipText = L("Bot is running. Click tray icon to manage.",
                                             "봇이 실행 중입니다. 트레이 아이콘을 클릭하여 관리하세요.");
                trayIcon.BalloonTipIcon = ToolTipIcon.Info;
                trayIcon.ShowBalloonTip(3000);
                trayIcon.BalloonTipClicked += (s3, e3) => { ShowControlPanel(); };
            }
            else if (waitCount > 10)
            {
                waitTimer.Stop();
                botStarting = false;
                try { File.Delete(vbs); } catch { }
                UpdateStatus();
                BuildMenu();
                RebuildControlPanel();
            }
        };
        waitTimer.Start();
    }

    private void KillBot()
    {
        // Kill ClaudeBot.exe process (copied from node.exe with custom name)
        try
        {
            foreach (var proc in Process.GetProcessesByName("ClaudeBot"))
            {
                try { proc.Kill(); proc.WaitForExit(3000); } catch { }
            }
        }
        catch { }
        // Fallback: kill any node.exe running dist/index.js (in case ClaudeBot.exe wasn't used)
        try
        {
            var proc = new Process();
            proc.StartInfo.FileName = "powershell";
            proc.StartInfo.Arguments = "-NoProfile -Command \"Get-WmiObject Win32_Process | Where-Object { $_.CommandLine -like '*dist/index.js*' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }\"";
            proc.StartInfo.UseShellExecute = false;
            proc.StartInfo.CreateNoWindow = true;
            proc.Start();
            proc.WaitForExit(10000);
        }
        catch { }
        string lockFile = Path.Combine(botDir, ".bot.lock");
        try { File.Delete(lockFile); } catch { }
    }

    private bool IsAutoStartEnabled()
    {
        try
        {
            var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run", false);
            if (key == null) return false;
            object val = key.GetValue(taskName);
            key.Close();
            return val != null;
        }
        catch { return false; }
    }

    private void ToggleAutoStart(object sender, EventArgs e)
    {
        try
        {
            var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(
                "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run", true);
            if (key == null) return;
            if (IsAutoStartEnabled())
            {
                key.DeleteValue(taskName, false);
            }
            else
            {
                key.SetValue(taskName, "\"" + Application.ExecutablePath + "\"");
            }
            key.Close();
        }
        catch { }
        BuildMenu();
    }

    private void StopBot(object sender, EventArgs e)
    {
        KillBot();
        Thread.Sleep(1000);
        UpdateStatus();
        BuildMenu();
    }

    private void RestartBot(object sender, EventArgs e)
    {
        KillBot();
        Thread.Sleep(2000);
        StartBot(null, null);
    }

    private void OpenLog(object sender, EventArgs e)
    {
        string logPath = Path.Combine(botDir, "bot.log");
        if (!File.Exists(logPath))
            File.Create(logPath).Close();
        Process.Start("notepad.exe", "\"" + logPath + "\"");
    }

    private void OpenFolder(object sender, EventArgs e)
    {
        Process.Start("explorer.exe", botDir);
    }

    private void OpenSettings(object sender, EventArgs e)
    {
        var env = LoadEnv();

        var form = new Form()
        {
            Text = L("Claude Discord Bot Settings", "Claude Discord Bot 설정"),
            Width = 500,
            Height = 520,
            StartPosition = FormStartPosition.CenterScreen,
            FormBorderStyle = FormBorderStyle.FixedDialog,
            MaximizeBox = false,
            MinimizeBox = false,
            BackColor = BgDark,
            ForeColor = FgWhite,
        };

        // Setup guide link
        var linkLabel = new LinkLabel() { Text = L("Open Setup Guide", "설정 가이드 열기"), Left = 15, Top = 10, Width = 450, Height = 20, LinkColor = LinkBlue, BackColor = Color.Transparent };
        linkLabel.LinkClicked += (s, ev) => { Process.Start("https://github.com/chadingTV/claudecode-discord/blob/main/SETUP.md"); };
        form.Controls.Add(linkLabel);

        // Issues link
        var issueLabel = new LinkLabel() { Text = L("Bug Report / Feature Request (GitHub Issues)", "버그 신고 / 기능 요청 (GitHub Issues)"), Left = 15, Top = 32, Width = 450, Height = 20, LinkColor = LinkBlue, BackColor = Color.Transparent };
        issueLabel.LinkClicked += (s, ev) => { Process.Start("https://github.com/chadingTV/claudecode-discord/issues"); };
        form.Controls.Add(issueLabel);

        string[][] fields = new string[][] {
            new string[] { "DISCORD_BOT_TOKEN", "Discord Bot Token" },
            new string[] { "DISCORD_GUILD_ID", "Discord Guild ID (Server ID)" },
            new string[] { "ALLOWED_USER_IDS", L("Allowed User IDs (comma-separated)", "허용된 사용자 ID (쉼표로 구분)") },
            new string[] { "BASE_PROJECT_DIR", L("Base Project Directory", "기본 프로젝트 디렉토리") },
            new string[] { "RATE_LIMIT_PER_MINUTE", L("Rate Limit Per Minute", "분당 요청 제한") },
        };

        string[] defaults = new string[] { "", "", "", "", "10" };
        string[] placeholders = new string[] {
            L("Paste your bot token here", "봇 토큰을 여기에 붙여넣으세요"),
            L("Right-click server > Copy Server ID", "서버 우클릭 > 서버 ID 복사"),
            L("e.g. 123456789,987654321", "예: 123456789,987654321"),
            L("e.g. C:\\Users\\you\\projects", "예: C:\\Users\\you\\projects"),
            "10"
        };
        // Placeholder values from .env.example that should be treated as empty
        var exampleValues = new System.Collections.Generic.HashSet<string>() {
            "your_bot_token_here", "your_server_id_here", "your_user_id_here",
            "/Users/yourname/projects", "/Users/you/projects", "C:\\Users\\yourname\\projects"
        };

        SetDarkTitleBar(form);

        var tbFont = new Font(FontFamily.GenericSansSerif, 10f);
        var textBoxes = new TextBox[fields.Length];
        int y = 58;

        for (int i = 0; i < fields.Length; i++)
        {
            var label = new Label() { Text = fields[i][1], Left = 15, Top = y, Width = 450, Font = new Font(FontFamily.GenericSansSerif, 9, FontStyle.Bold), ForeColor = FgWhite, BackColor = Color.Transparent };
            form.Controls.Add(label);
            y += 22;

            if (fields[i][0] == "BASE_PROJECT_DIR")
            {
                var tb = new TextBox() { Left = 15, Top = y, Width = 360, BackColor = BgPanel, ForeColor = FgWhite, BorderStyle = BorderStyle.FixedSingle, Font = tbFont };
                string val = "";
                env.TryGetValue(fields[i][0], out val);
                if (val != null && exampleValues.Contains(val)) val = "";
                if (val != null && val != "")
                    tb.Text = val;
                else
                {
                    string hint = placeholders[i];
                    tb.HandleCreated += (s2, e2) => {
                        SendMessage(((TextBox)s2).Handle, EM_SETCUEBANNER, IntPtr.Zero, hint);
                    };
                }
                form.Controls.Add(tb);
                textBoxes[i] = tb;

                var browseBtn = new Button() { Text = L("Browse...", "찾아보기..."), Left = 380, Top = y, Width = 85, Height = tb.Height, FlatStyle = FlatStyle.Flat, BackColor = BgButton, ForeColor = FgWhite };
                browseBtn.FlatAppearance.BorderSize = 0;
                int idx = i;
                browseBtn.Click += (s, ev) =>
                {
                    using (var fbd = new FolderBrowserDialog())
                    {
                        fbd.Description = L("Select Base Project Directory", "기본 프로젝트 디렉토리 선택");
                        if (textBoxes[idx].Text != "") fbd.SelectedPath = textBoxes[idx].Text;
                        if (fbd.ShowDialog() == DialogResult.OK)
                        {
                            textBoxes[idx].Text = fbd.SelectedPath;
                        }
                    }
                };
                form.Controls.Add(browseBtn);
            }
            else
            {
                var tb = new TextBox() { Left = 15, Top = y, Width = 450, BackColor = BgPanel, ForeColor = FgWhite, BorderStyle = BorderStyle.FixedSingle, Font = tbFont };
                string val = "";
                env.TryGetValue(fields[i][0], out val);
                if (val != null && exampleValues.Contains(val)) val = "";

                if (fields[i][0] == "DISCORD_BOT_TOKEN" && val != null && val.Length > 10)
                {
                    tb.HandleCreated += (s2, e2) => {
                        SendMessage(((TextBox)s2).Handle, EM_SETCUEBANNER, IntPtr.Zero,
                            "****" + val.Substring(val.Length - 6) + L(" (enter full token to change)", " (변경하려면 전체 토큰 입력)"));
                    };
                }
                else if (val != null && val != "")
                {
                    tb.Text = val;
                }
                else
                {
                    tb.Text = defaults[i];
                    if (defaults[i] == "")
                    {
                        string hint = placeholders[i];
                        tb.HandleCreated += (s2, e2) => {
                            SendMessage(((TextBox)s2).Handle, EM_SETCUEBANNER, IntPtr.Zero, hint);
                        };
                    }
                }

                form.Controls.Add(tb);
                textBoxes[i] = tb;
            }
            y += 34;
        }

        // Show Cost radio buttons
        var showCostLabel = new Label() { Text = L("Show Cost", "비용 표시"), Left = 15, Top = y, Width = 450, Font = new Font(FontFamily.GenericSansSerif, 9, FontStyle.Bold), ForeColor = FgWhite, BackColor = Color.Transparent };
        form.Controls.Add(showCostLabel);
        y += 22;

        string showCostVal = "true";
        env.TryGetValue("SHOW_COST", out showCostVal);
        if (showCostVal == null) showCostVal = "true";
        bool showCostEnabled = (showCostVal.ToLower() != "false");

        var radioTrue = new RadioButton() { Text = "True", Left = 15, Top = y, Width = 80, Height = 22, ForeColor = FgWhite, BackColor = Color.Transparent, Font = new Font(FontFamily.GenericSansSerif, 9.5f), Checked = showCostEnabled };
        var radioFalse = new RadioButton() { Text = "False", Left = 100, Top = y, Width = 80, Height = 22, ForeColor = FgWhite, BackColor = Color.Transparent, Font = new Font(FontFamily.GenericSansSerif, 9.5f), Checked = !showCostEnabled };
        var costNote = new Label() { Text = L("(set False for Max plan)", "(Max 요금제는 False)"), Left = 190, Top = y + 2, Width = 250, Height = 20, ForeColor = FgDimGray, BackColor = Color.Transparent, Font = new Font(FontFamily.GenericSansSerif, 8.5f) };
        form.Controls.Add(radioTrue);
        form.Controls.Add(radioFalse);
        form.Controls.Add(costNote);
        y += 34;

        var saveBtn = MakeDarkButton(L("Save", "저장"), 300, y, 80, 32, AccentBlue, Color.White);
        var cancelBtn = MakeDarkButton(L("Cancel", "취소"), 385, y, 80, 32, BgButton, FgWhite);

        saveBtn.Click += (s, ev) =>
        {
            string[] values = new string[fields.Length];
            for (int i = 0; i < fields.Length; i++)
            {
                values[i] = textBoxes[i].Text.Trim();
                if (values[i] == "" && fields[i][0] == "DISCORD_BOT_TOKEN")
                {
                    string existing = "";
                    env.TryGetValue(fields[i][0], out existing);
                    values[i] = existing ?? "";
                }
                if (values[i] == "") values[i] = defaults[i];
            }

            if (values[0] == "" || values[1] == "" || values[2] == "")
            {
                MessageBox.Show(
                    L("Bot Token, Guild ID (Server ID), and User IDs are required.",
                      "Bot Token, Guild ID (서버 ID), User IDs는 필수 항목입니다."),
                    L("Required Fields Missing", "필수 항목 누락"), MessageBoxButtons.OK, MessageBoxIcon.Warning);
                return;
            }

            using (var sw = new StreamWriter(envPath))
            {
                for (int i = 0; i < fields.Length; i++)
                {
                    sw.WriteLine(fields[i][0] + "=" + values[i]);
                }
                sw.WriteLine("# Show estimated API cost in task results (set false for Max plan users)");
                sw.WriteLine("SHOW_COST=" + (radioTrue.Checked ? "true" : "false"));
            }

            form.DialogResult = DialogResult.OK;
            form.Close();
        };

        cancelBtn.Click += (s, ev) => { form.Close(); };

        form.Controls.Add(saveBtn);
        form.Controls.Add(cancelBtn);
        form.AcceptButton = saveBtn;
        form.CancelButton = cancelBtn;
        form.ShowDialog();

        UpdateStatus();
        BuildMenu();
    }

    private System.Collections.Generic.Dictionary<string, string> LoadEnv()
    {
        var env = new System.Collections.Generic.Dictionary<string, string>();
        if (!File.Exists(envPath)) return env;

        foreach (var line in File.ReadAllLines(envPath))
        {
            string trimmed = line.Trim();
            if (trimmed.StartsWith("#") || !trimmed.Contains("=")) continue;
            int idx = trimmed.IndexOf('=');
            string key = trimmed.Substring(0, idx);
            string val = trimmed.Substring(idx + 1);
            env[key] = val;
        }
        return env;
    }

    private Form controlPanel = null;

    // Dark theme colors
    private static readonly Color BgDark = Color.FromArgb(42, 42, 42);
    private static readonly Color BgPanel = Color.FromArgb(58, 58, 58);
    private static readonly Color BgButton = Color.FromArgb(72, 72, 72);
    private static readonly Color FgWhite = Color.FromArgb(230, 230, 230);
    private static readonly Color FgGray = Color.FromArgb(150, 150, 150);
    private static readonly Color FgDimGray = Color.FromArgb(110, 110, 110);
    private static readonly Color SepColor = Color.FromArgb(65, 65, 65);
    private static readonly Color BtnStop = Color.FromArgb(140, 60, 60);
    private static readonly Color BtnRestart = Color.FromArgb(130, 110, 55);
    private static readonly Color BtnSettings = Color.FromArgb(50, 70, 130);
    private static readonly Color LinkBlue = Color.FromArgb(100, 160, 240);
    private static readonly Color AccentBlue = Color.FromArgb(66, 133, 244);

    private static GraphicsPath RoundedRect(Rectangle bounds, int radius)
    {
        var path = new GraphicsPath();
        int d = radius * 2;
        path.AddArc(bounds.X, bounds.Y, d, d, 180, 90);
        path.AddArc(bounds.Right - d, bounds.Y, d, d, 270, 90);
        path.AddArc(bounds.Right - d, bounds.Bottom - d, d, d, 0, 90);
        path.AddArc(bounds.X, bounds.Bottom - d, d, d, 90, 90);
        path.CloseFigure();
        return path;
    }

    private static void SetDarkTitleBar(Form form)
    {
        try
        {
            int value = 1;
            DwmSetWindowAttribute(form.Handle, DWMWA_USE_IMMERSIVE_DARK_MODE, ref value, sizeof(int));
        }
        catch { }
    }

    private Button MakeDarkButton(string text, int left, int top, int width, int height, Color bgColor, Color fgColor)
    {
        var btn = new Button()
        {
            Text = text, Left = left, Top = top, Width = width, Height = height,
            FlatStyle = FlatStyle.Flat, BackColor = bgColor, ForeColor = fgColor,
            Font = new Font(FontFamily.GenericSansSerif, 9.5f, FontStyle.Bold),
            Cursor = Cursors.Hand
        };
        btn.FlatAppearance.BorderSize = 0;
        btn.FlatAppearance.MouseOverBackColor = Color.FromArgb(
            Math.Min(bgColor.R + 20, 255), Math.Min(bgColor.G + 20, 255), Math.Min(bgColor.B + 20, 255));
        using (var path = RoundedRect(new Rectangle(0, 0, width, height), 8))
            btn.Region = new Region(path);
        return btn;
    }

    private void ShowControlPanel()
    {
        // If already open, bring to front
        if (controlPanel != null && !controlPanel.IsDisposed)
        {
            controlPanel.Activate();
            return;
        }

        int panelWidth = 460;

        controlPanel = new Form()
        {
            Text = "Claude Discord Bot",
            Width = panelWidth,
            Height = 590,
            StartPosition = FormStartPosition.CenterScreen,
            FormBorderStyle = FormBorderStyle.FixedDialog,
            MaximizeBox = false,
            MinimizeBox = false,
            BackColor = BgDark,
            ForeColor = FgWhite,
        };

        string icoPath = Path.Combine(botDir, "docs", "icon.ico");
        if (File.Exists(icoPath))
            controlPanel.Icon = new Icon(icoPath);

        SetDarkTitleBar(controlPanel);

        RebuildControlPanel();
        // Fetch usage if stale (>5 min) when panel opens
        if (usageLastFetched == null || (DateTime.Now - usageLastFetched.Value).TotalSeconds > 300)
        {
            new Thread(() => { try { FetchUsage(); } catch { } }) { IsBackground = true }.Start();
        }
        controlPanel.ShowDialog();
        controlPanel = null;
    }

    private void RebuildControlPanel()
    {
        if (controlPanel == null || controlPanel.IsDisposed) return;

        // Remember position
        Point pos = controlPanel.Location;
        bool wasVisible = controlPanel.Visible;
        lastPanelRunning = IsRunning();

        controlPanel.SuspendLayout();
        controlPanel.Controls.Clear();

        bool running = IsRunning();
        bool hasEnv = IsEnvConfigured();

        int panelWidth = controlPanel.Width;
        int btnWidth = panelWidth - 60;
        int halfBtnWidth = (btnWidth - 10) / 2;

        int y = 20;

        // Header: Icon + Title + Version + Language toggle
        string iconPngPath = Path.Combine(botDir, "docs", "icon-rounded.png");
        if (File.Exists(iconPngPath))
        {
            var iconBox = new PictureBox()
            {
                Left = 25, Top = y, Width = 48, Height = 48,
                SizeMode = PictureBoxSizeMode.Zoom,
                BackColor = Color.Transparent
            };
            try { iconBox.Image = Image.FromFile(iconPngPath); } catch { }
            controlPanel.Controls.Add(iconBox);
        }

        var titleLabel = new Label()
        {
            Text = "Claude Discord Bot",
            Left = 82, Top = y,
            Width = 250, Height = 24,
            Font = new Font(FontFamily.GenericSansSerif, 14, FontStyle.Bold),
            ForeColor = FgWhite, BackColor = Color.Transparent
        };
        controlPanel.Controls.Add(titleLabel);

        var verSubLabel = new Label()
        {
            Text = currentVersion,
            Left = 82, Top = y + 26,
            Width = 250, Height = 16,
            Font = new Font(FontFamily.GenericSansSerif, 9),
            ForeColor = FgGray, BackColor = Color.Transparent
        };
        controlPanel.Controls.Add(verSubLabel);

        // Language toggle - top right
        var enBtn = new Label()
        {
            Text = "EN",
            Left = panelWidth - 110, Top = y + 6,
            Width = 32, Height = 22,
            TextAlign = ContentAlignment.MiddleCenter,
            Font = new Font(FontFamily.GenericSansSerif, 9, !isKorean ? FontStyle.Bold : FontStyle.Regular),
            ForeColor = !isKorean ? Color.White : FgDimGray,
            BackColor = !isKorean ? AccentBlue : BgButton,
            Cursor = Cursors.Hand
        };
        var divLabel = new Label()
        {
            Text = "|",
            Left = panelWidth - 78, Top = y + 6,
            Width = 10, Height = 22,
            TextAlign = ContentAlignment.MiddleCenter,
            ForeColor = SepColor, BackColor = Color.Transparent
        };
        var krBtn = new Label()
        {
            Text = "KR",
            Left = panelWidth - 68, Top = y + 6,
            Width = 32, Height = 22,
            TextAlign = ContentAlignment.MiddleCenter,
            Font = new Font(FontFamily.GenericSansSerif, 9, isKorean ? FontStyle.Bold : FontStyle.Regular),
            ForeColor = isKorean ? Color.White : FgDimGray,
            BackColor = isKorean ? AccentBlue : BgButton,
            Cursor = Cursors.Hand
        };
        enBtn.Click += (s, ev) => { SetLanguage(false); RebuildControlPanel(); };
        krBtn.Click += (s, ev) => { SetLanguage(true); RebuildControlPanel(); };
        controlPanel.Controls.Add(enBtn);
        controlPanel.Controls.Add(divLabel);
        controlPanel.Controls.Add(krBtn);

        y += 58;

        // Separator after header
        var headerSep = new Label() { Left = 25, Top = y, Width = btnWidth, Height = 1, BackColor = SepColor };
        controlPanel.Controls.Add(headerSep);
        y += 15;

        // Status indicator
        string statusText = !hasEnv ? L("Setup Required", "설정 필요") : botStarting ? L("Starting...", "시작 중...") : (running ? L("Running", "실행 중") : L("Stopped", "중지됨"));
        Color statusColor = !hasEnv ? Color.Orange : botStarting ? Color.Yellow : (running ? Color.LimeGreen : Color.Red);
        var statusPanel = new Panel() { Left = 25, Top = y, Width = btnWidth, Height = 50, BackColor = BgPanel };
        using (var path = RoundedRect(new Rectangle(0, 0, btnWidth, 50), 8))
            statusPanel.Region = new Region(path);
        var statusDot = new Label() { Left = 14, Top = 14, Width = 24, Height = 24, Text = "", BackColor = Color.Transparent };
        statusDot.Paint += (s, e) => {
            e.Graphics.SmoothingMode = System.Drawing.Drawing2D.SmoothingMode.AntiAlias;
            using (var brush = new SolidBrush(statusColor))
                e.Graphics.FillEllipse(brush, 2, 2, 18, 18);
        };
        statusPanel.Controls.Add(statusDot);
        var statusLabel = new Label() {
            Left = 42, Top = 15, Width = 320, Height = 24,
            Text = statusText,
            Font = new Font(FontFamily.GenericSansSerif, 12, FontStyle.Bold),
            ForeColor = FgWhite, BackColor = Color.Transparent
        };
        statusPanel.Controls.Add(statusLabel);
        controlPanel.Controls.Add(statusPanel);
        y += 62;

        // Claude Code Usage section
        if (usageFiveHour >= 0 || usageSevenDay >= 0 || usageSevenDaySonnet >= 0)
        {
            EventHandler openUsagePage = (s, ev) => { Process.Start("https://claude.ai/settings/usage"); };

            var usageLabel = new Label()
            {
                Text = L("Claude Code Usage", "Claude Code 사용량"),
                Left = 25, Top = y, Width = btnWidth, Height = 20,
                Font = new Font(FontFamily.GenericSansSerif, 9.5f, FontStyle.Bold),
                ForeColor = FgWhite, BackColor = Color.Transparent,
                Cursor = Cursors.Hand
            };
            usageLabel.Click += openUsagePage;
            controlPanel.Controls.Add(usageLabel);
            y += 24;

            // Usage bars
            string[][] usageItems = new string[][] {
                new string[] { L("Session 5hr", "세션 5시간"), usageFiveHour.ToString("F3"), usageFiveHourReset },
                new string[] { L("Weekly 7day", "주간 7일"), usageSevenDay.ToString("F3"), usageSevenDayReset },
                new string[] { L("Weekly Sonnet", "주간 Sonnet"), usageSevenDaySonnet.ToString("F3"), usageSevenDaySonnetReset }
            };
            double[] usageValues = new double[] { usageFiveHour, usageSevenDay, usageSevenDaySonnet };

            for (int i = 0; i < 3; i++)
            {
                if (usageValues[i] < 0) continue;
                double pct = usageValues[i];
                int pctInt = (int)(pct * 100);
                Color barColor = pct < 0.5 ? AccentBlue : pct < 0.8 ? Color.FromArgb(220, 160, 50) : Color.FromArgb(220, 80, 80);

                // Label + percentage
                string resetInfo = usageItems[i][2].Length > 0 ? "  (" + usageItems[i][2] + ")" : "";
                var itemLabel = new Label()
                {
                    Text = usageItems[i][0] + ":  " + pctInt + "%" + resetInfo,
                    Left = 25, Top = y, Width = btnWidth, Height = 16,
                    Font = new Font(FontFamily.GenericSansSerif, 8.5f),
                    ForeColor = FgGray, BackColor = Color.Transparent,
                    Cursor = Cursors.Hand
                };
                itemLabel.Click += openUsagePage;
                controlPanel.Controls.Add(itemLabel);
                y += 18;

                // Progress bar background
                int barWidth = btnWidth;
                int barHeight = 10;
                var barBg = new Panel() { Left = 25, Top = y, Width = barWidth, Height = barHeight, BackColor = BgPanel, Cursor = Cursors.Hand };
                using (var rr = RoundedRect(new Rectangle(0, 0, barWidth, barHeight), 4))
                    barBg.Region = new Region(rr);
                barBg.Click += openUsagePage;
                controlPanel.Controls.Add(barBg);

                // Progress bar fill
                int fillWidth = Math.Max(1, (int)(barWidth * Math.Min(pct, 1.0)));
                var barFill = new Panel() { Left = 0, Top = 0, Width = fillWidth, Height = barHeight, BackColor = barColor, Cursor = Cursors.Hand };
                if (fillWidth >= 8)
                {
                    using (var rr = RoundedRect(new Rectangle(0, 0, fillWidth, barHeight), 4))
                        barFill.Region = new Region(rr);
                }
                barFill.Click += openUsagePage;
                barBg.Controls.Add(barFill);

                y += 18;
            }

            // Last fetched time
            string lastFetchedText = FormatLastFetched();
            if (lastFetchedText.Length > 0)
            {
                var fetchedLabel = new Label()
                {
                    Text = lastFetchedText,
                    Left = 25, Top = y, Width = btnWidth, Height = 14,
                    Font = new Font(FontFamily.GenericSansSerif, 7.5f),
                    ForeColor = FgDimGray, BackColor = Color.Transparent,
                    TextAlign = ContentAlignment.MiddleRight,
                    Cursor = Cursors.Hand
                };
                fetchedLabel.Click += openUsagePage;
                controlPanel.Controls.Add(fetchedLabel);
                y += 16;
            }

            // Refresh usage button
            var refreshUsageBtn = MakeDarkButton(L("Refresh Usage", "사용량 새로고침"), 25, y, btnWidth, 32, BgButton, FgGray);
            refreshUsageBtn.Font = new Font(FontFamily.GenericSansSerif, 8.5f);
            refreshUsageBtn.Click += (s, ev) => {
                FetchUsage();
            };
            controlPanel.Controls.Add(refreshUsageBtn);
            y += 42;
        }
        else
        {
            // Show fetch button when no data yet
            var fetchUsageBtn = MakeDarkButton(L("Load Usage Info", "사용량 정보 불러오기"), 25, y, btnWidth, 36, BgButton, FgWhite);
            fetchUsageBtn.Click += (s, ev) => { FetchUsage(true); };
            controlPanel.Controls.Add(fetchUsageBtn);
            y += 46;
        }

        // Bot control buttons
        if (hasEnv)
        {
            if (running)
            {
                var stopBtn = MakeDarkButton(L("Stop Bot", "봇 중지"), 25, y, halfBtnWidth, 42, BtnStop, Color.FromArgb(230, 120, 120));
                stopBtn.Click += (s, ev) => { StopBot(null, null); RebuildControlPanel(); };
                controlPanel.Controls.Add(stopBtn);

                var restartBtn = MakeDarkButton(L("Restart Bot", "봇 재시작"), 25 + halfBtnWidth + 10, y, halfBtnWidth, 42, BtnRestart, Color.FromArgb(220, 180, 90));
                restartBtn.Click += (s, ev) => { RestartBot(null, null); };
                controlPanel.Controls.Add(restartBtn);
            }
            else if (botStarting)
            {
                var startingBtn = MakeDarkButton(L("Starting...", "시작 중..."), 25, y, btnWidth, 42, BgPanel, FgDimGray);
                startingBtn.Enabled = false;
                controlPanel.Controls.Add(startingBtn);
            }
            else
            {
                var startBtn = MakeDarkButton(L("Start Bot", "봇 시작"), 25, y, btnWidth, 42, BgButton, FgWhite);
                startBtn.Click += (s, ev) => { StartBot(null, null); };
                controlPanel.Controls.Add(startBtn);
            }
            y += 52;
        }

        // Settings button
        var settingsBtn = MakeDarkButton(L("Settings...", "설정..."), 25, y, btnWidth, 42, BtnSettings, Color.FromArgb(100, 160, 240));
        settingsBtn.Click += (s, ev) => {
            OpenSettings(null, null);
            UpdateStatus();
            BuildMenu();
        };
        controlPanel.Controls.Add(settingsBtn);
        y += 50;

        if (hasEnv)
        {
            // View Log
            var logBtn = MakeDarkButton(L("View Log", "로그 보기"), 25, y, halfBtnWidth, 42, BgButton, FgWhite);
            logBtn.Click += (s, ev) => { OpenLog(null, null); };
            controlPanel.Controls.Add(logBtn);

            // Open Folder
            var folderBtn = MakeDarkButton(L("Open Folder", "폴더 열기"), 25 + halfBtnWidth + 10, y, halfBtnWidth, 42, BgButton, FgWhite);
            folderBtn.Click += (s, ev) => { OpenFolder(null, null); };
            controlPanel.Controls.Add(folderBtn);
            y += 52;
        }

        // Separator line
        var sep1 = new Label() { Left = 25, Top = y, Width = btnWidth, Height = 1, BackColor = SepColor };
        controlPanel.Controls.Add(sep1);
        y += 12;

        // Auto-start checkbox
        var autoCheck = new CheckBox()
        {
            Text = L("Auto Run on Startup", "시작 시 자동 실행"),
            Left = 25, Top = y, Width = btnWidth, Height = 22,
            Font = new Font(FontFamily.GenericSansSerif, 9.5f),
            ForeColor = FgWhite, BackColor = Color.Transparent,
            Checked = IsAutoStartEnabled()
        };
        autoCheck.CheckedChanged += (s, ev) => { ToggleAutoStart(null, null); };
        controlPanel.Controls.Add(autoCheck);
        y += 36;

        // Update button
        if (updateAvailable)
        {
            var updateBtn = MakeDarkButton(
                L("Update Available - Click to Update", "업데이트 가능 - 클릭하여 업데이트"),
                25, y, btnWidth, 42, AccentBlue, Color.White);
            updateBtn.Click += (s, ev) => { controlPanel.Close(); PerformUpdate(null, null); };
            controlPanel.Controls.Add(updateBtn);
            y += 52;
        }
        else
        {
            var checkUpdateBtn = MakeDarkButton(L("Check for Updates", "업데이트 확인"), 25, y, btnWidth, 42, BgButton, FgWhite);
            checkUpdateBtn.Click += (s, ev) => {
                CheckForUpdates();
                if (updateAvailable)
                {
                    RebuildControlPanel();
                }
                else
                {
                    MessageBox.Show(
                        L("You are running the latest version.", "최신 버전을 사용 중입니다."),
                        L("No Updates", "업데이트 없음"),
                        MessageBoxButtons.OK, MessageBoxIcon.Information);
                }
            };
            controlPanel.Controls.Add(checkUpdateBtn);
            y += 52;
        }

        // Separator line
        var sep2 = new Label() { Left = 25, Top = y, Width = btnWidth, Height = 1, BackColor = SepColor };
        controlPanel.Controls.Add(sep2);
        y += 12;

        // Info message
        var infoLabel = new Label() {
            Text = L("Closing this window does not stop the bot.\nThe bot runs in the background. Check the tray icon for status.",
                      "이 창을 닫아도 봇은 중지되지 않습니다.\n봇은 백그라운드에서 실행됩니다. 트레이 아이콘에서 상태를 확인하세요."),
            Left = 25, Top = y, Width = btnWidth, Height = 40,
            ForeColor = FgDimGray, BackColor = Color.Transparent,
            Font = new Font(FontFamily.GenericSansSerif, 8.5f)
        };
        controlPanel.Controls.Add(infoLabel);
        y += 48;

        // Quit button
        var quitBtn = MakeDarkButton(L("Quit Bot", "봇 종료"), 25, y, btnWidth, 42, BgButton, FgGray);
        quitBtn.Click += (s, ev) => { controlPanel.Close(); QuitAll(null, null); };
        controlPanel.Controls.Add(quitBtn);
        y += 52;

        // Separator line
        var sep3 = new Label() { Left = 25, Top = y, Width = btnWidth, Height = 1, BackColor = SepColor };
        controlPanel.Controls.Add(sep3);
        y += 12;

        // GitHub link
        var ghLink = new LinkLabel()
        {
            Text = "GitHub: chadingTV/claudecode-discord",
            Left = 25, Top = y, Width = btnWidth, Height = 20,
            TextAlign = ContentAlignment.MiddleCenter,
            Font = new Font(FontFamily.GenericSansSerif, 8.5f),
            LinkColor = LinkBlue, BackColor = Color.Transparent
        };
        ghLink.LinkClicked += (s, ev) => { Process.Start("https://github.com/chadingTV/claudecode-discord"); };
        controlPanel.Controls.Add(ghLink);
        y += 22;

        // Issues link
        var issueLink = new LinkLabel()
        {
            Text = L("Bug Report / Feature Request (GitHub Issues)", "버그 신고 / 기능 요청 (GitHub Issues)"),
            Left = 25, Top = y, Width = btnWidth, Height = 20,
            TextAlign = ContentAlignment.MiddleCenter,
            Font = new Font(FontFamily.GenericSansSerif, 8.5f),
            LinkColor = LinkBlue, BackColor = Color.Transparent
        };
        issueLink.LinkClicked += (s, ev) => { Process.Start("https://github.com/chadingTV/claudecode-discord/issues"); };
        controlPanel.Controls.Add(issueLink);
        y += 22;

        // Star request
        var starLabel = new Label()
        {
            Text = L("If you find this useful, please give it a Star on GitHub!",
                      "유용하셨다면 GitHub에서 Star를 눌러주세요!"),
            Left = 25, Top = y, Width = btnWidth, Height = 18,
            TextAlign = ContentAlignment.MiddleCenter,
            Font = new Font(FontFamily.GenericSansSerif, 8f),
            ForeColor = FgDimGray, BackColor = Color.Transparent
        };
        controlPanel.Controls.Add(starLabel);
        y += 28;

        controlPanel.Height = y + 45;

        // Restore position if it was already visible
        if (wasVisible)
        {
            controlPanel.Location = pos;
        }

        controlPanel.ResumeLayout(true);
    }

    private bool RefreshOAuthToken(string credPath, string credJson)
    {
        try
        {
            var refreshMatch = Regex.Match(credJson, "\"refreshToken\"\\s*:\\s*\"([^\"]+)\"");
            if (!refreshMatch.Success) return false;
            string refreshToken = refreshMatch.Groups[1].Value;

            string postData = "grant_type=refresh_token"
                + "&refresh_token=" + Uri.EscapeDataString(refreshToken)
                + "&client_id=" + Uri.EscapeDataString("9d1c250a-e61b-44d9-88ed-5944d1962f5e")
                + "&scope=" + Uri.EscapeDataString("user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload");

            var request = (HttpWebRequest)WebRequest.Create("https://platform.claude.com/v1/oauth/token");
            request.Method = "POST";
            request.Timeout = 15000;
            request.ContentType = "application/x-www-form-urlencoded";
            byte[] data = System.Text.Encoding.UTF8.GetBytes(postData);
            request.ContentLength = data.Length;
            using (var stream = request.GetRequestStream())
                stream.Write(data, 0, data.Length);

            using (var response = (HttpWebResponse)request.GetResponse())
            using (var reader = new StreamReader(response.GetResponseStream()))
            {
                string json = reader.ReadToEnd();

                var newAccessMatch = Regex.Match(json, "\"access_token\"\\s*:\\s*\"([^\"]+)\"");
                var newRefreshMatch = Regex.Match(json, "\"refresh_token\"\\s*:\\s*\"([^\"]+)\"");
                var expiresInMatch = Regex.Match(json, "\"expires_in\"\\s*:\\s*(\\d+)");
                if (!newAccessMatch.Success) return false;

                string newAccess = newAccessMatch.Groups[1].Value;
                string newRefresh = newRefreshMatch.Success ? newRefreshMatch.Groups[1].Value : refreshToken;
                long newExpiresAt = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() + (expiresInMatch.Success ? long.Parse(expiresInMatch.Groups[1].Value) * 1000 : 3600000);

                // Update credentials.json
                string updated = credJson;
                updated = Regex.Replace(updated, "\"accessToken\"\\s*:\\s*\"[^\"]+\"", "\"accessToken\":\"" + newAccess + "\"");
                updated = Regex.Replace(updated, "\"refreshToken\"\\s*:\\s*\"[^\"]+\"", "\"refreshToken\":\"" + newRefresh + "\"");
                updated = Regex.Replace(updated, "\"expiresAt\"\\s*:\\s*\\d+", "\"expiresAt\":" + newExpiresAt);
                File.WriteAllText(credPath, updated);
                return true;
            }
        }
        catch (Exception ex)
        {
            try { File.AppendAllText(Path.Combine(botDir, "usage-error.log"), DateTime.Now + ": OAuth refresh failed: " + ex.Message + "\n"); } catch { }
            return false;
        }
    }

    private bool IsTokenExpired(string credJson)
    {
        var expiresMatch = Regex.Match(credJson, "\"expiresAt\"\\s*:\\s*(\\d+)");
        if (!expiresMatch.Success) return false;
        long expiresAt = long.Parse(expiresMatch.Groups[1].Value);
        long nowMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        return nowMs >= (expiresAt - 300000); // 5분 여유
    }

    private void FetchUsage(bool openPageOnFail = false)
    {
        bool success = false;
        try
        {
            ServicePointManager.SecurityProtocol = SecurityProtocolType.Tls12 | SecurityProtocolType.Tls13;
            string home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            string credPath = Path.Combine(home, ".claude", ".credentials.json");
            if (!File.Exists(credPath)) { if (openPageOnFail) Process.Start("https://claude.ai/settings/usage"); return; }

            string credJson = File.ReadAllText(credPath);

            // 토큰 만료 시 자동 갱신
            if (IsTokenExpired(credJson))
            {
                if (RefreshOAuthToken(credPath, credJson))
                    credJson = File.ReadAllText(credPath);
            }

            var tokenMatch = Regex.Match(credJson, "\"accessToken\"\\s*:\\s*\"([^\"]+)\"");
            if (!tokenMatch.Success) { if (openPageOnFail) Process.Start("https://claude.ai/settings/usage"); return; }
            string token = tokenMatch.Groups[1].Value;

            string usageJson = null;
            try
            {
                usageJson = FetchUsageApi(token);
            }
            catch (WebException wex)
            {
                var httpResp = wex.Response as HttpWebResponse;
                if (httpResp != null && (httpResp.StatusCode == HttpStatusCode.Unauthorized || (int)httpResp.StatusCode == 429))
                {
                    // 401/429 → 토큰 갱신 후 재시도
                    if (RefreshOAuthToken(credPath, credJson))
                    {
                        credJson = File.ReadAllText(credPath);
                        tokenMatch = Regex.Match(credJson, "\"accessToken\"\\s*:\\s*\"([^\"]+)\"");
                        if (tokenMatch.Success)
                            usageJson = FetchUsageApi(tokenMatch.Groups[1].Value);
                    }
                }
                if (usageJson == null) throw;
            }

            ParseUsageJson(usageJson);
            usageLastFetched = DateTime.Now;
            SaveUsageCache(usageJson);
            success = true;

            // Refresh panel if open
            if (controlPanel != null && !controlPanel.IsDisposed)
            {
                if (controlPanel.InvokeRequired)
                    controlPanel.Invoke(new Action(() => RebuildControlPanel()));
                else
                    RebuildControlPanel();
            }
        }
        catch (Exception ex)
        {
            try { File.AppendAllText(Path.Combine(botDir, "usage-error.log"), DateTime.Now + ": " + ex.Message + "\n"); } catch { }
            if (openPageOnFail) Process.Start("https://claude.ai/settings/usage");
        }
    }

    private string FetchUsageApi(string token)
    {
        var request = (HttpWebRequest)WebRequest.Create("https://api.anthropic.com/api/oauth/usage");
        request.Method = "GET";
        request.Timeout = 10000;
        request.Headers.Add("Authorization", "Bearer " + token);
        request.Headers.Add("anthropic-beta", "oauth-2025-04-20");

        using (var response = (HttpWebResponse)request.GetResponse())
        using (var reader = new StreamReader(response.GetResponseStream()))
            return reader.ReadToEnd();
    }

    private void ParseUsageJson(string json)
    {
        // Parse five_hour, seven_day, seven_day_sonnet utilization and resets_at
        usageFiveHour = ParseUtilization(json, "five_hour");
        usageSevenDay = ParseUtilization(json, "seven_day");
        usageSevenDaySonnet = ParseUtilization(json, "seven_day_sonnet");
        usageFiveHourReset = ParseResetTime(json, "five_hour");
        usageSevenDayReset = ParseResetTime(json, "seven_day");
        usageSevenDaySonnetReset = ParseResetTime(json, "seven_day_sonnet");
    }

    private int FindSectionIndex(string json, string section)
    {
        // For "seven_day", must not match "seven_day_sonnet"
        string pattern = "\"" + Regex.Escape(section) + "\"\\s*:";
        var m = Regex.Match(json, pattern);
        if (!m.Success) return -1;
        return m.Index;
    }

    private double ParseUtilization(string json, string section)
    {
        int idx = FindSectionIndex(json, section);
        if (idx < 0) return -1;
        string sub = json.Substring(idx, Math.Min(200, json.Length - idx));
        var m = Regex.Match(sub, "\"utilization\"\\s*:\\s*([\\d.]+)");
        if (!m.Success) return -1;
        double val;
        if (double.TryParse(m.Groups[1].Value, NumberStyles.Float, CultureInfo.InvariantCulture, out val))
            return val / 100.0;
        return -1;
    }

    private string ParseResetTime(string json, string section)
    {
        int idx = FindSectionIndex(json, section);
        if (idx < 0) return "";
        string sub = json.Substring(idx, Math.Min(300, json.Length - idx));
        var m = Regex.Match(sub, "\"resets_at\"\\s*:\\s*\"([^\"]+)\"");
        if (!m.Success) return "";
        return FormatResetTime(m.Groups[1].Value);
    }

    private string FormatResetTime(string iso8601)
    {
        try
        {
            var resetTime = DateTime.Parse(iso8601, null, DateTimeStyles.RoundtripKind);
            var diff = resetTime.ToUniversalTime() - DateTime.UtcNow;
            if (diff.TotalMinutes < 1) return L("soon", "곧");
            if (diff.TotalHours < 1) return string.Format(L("Reset in {0}m", "{0}분 후 초기화"), (int)diff.TotalMinutes);
            return string.Format(L("Reset in {0}h", "{0}시간 후 초기화"), (int)Math.Ceiling(diff.TotalHours));
        }
        catch { return ""; }
    }

    private string UsageCachePath
    {
        get { return Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), ".claude", ".usage-cache.json"); }
    }

    private void SaveUsageCache(string json)
    {
        try
        {
            // Append _fetched_at timestamp to the raw JSON
            string timestamp = DateTime.UtcNow.ToString("o");
            string cached = json.TrimEnd().TrimEnd('}') + ",\"_fetched_at\":\"" + timestamp + "\"}";
            File.WriteAllText(UsageCachePath, cached);
        }
        catch { }
    }

    private void LoadUsageCache()
    {
        try
        {
            if (!File.Exists(UsageCachePath)) return;
            string json = File.ReadAllText(UsageCachePath);
            ParseUsageJson(json);

            // Parse _fetched_at
            var m = Regex.Match(json, "\"_fetched_at\"\\s*:\\s*\"([^\"]+)\"");
            if (m.Success)
            {
                DateTime dt;
                if (DateTime.TryParse(m.Groups[1].Value, null, DateTimeStyles.RoundtripKind, out dt))
                    usageLastFetched = dt.ToLocalTime();
            }
        }
        catch { }
    }

    private string FormatLastFetched()
    {
        if (usageLastFetched == null) return "";
        var ago = (int)(DateTime.Now - usageLastFetched.Value).TotalSeconds;
        if (ago < 60) return L("Updated just now", "방금 갱신됨");
        if (ago < 3600) return string.Format(L("Updated {0}m ago", "{0}분 전 갱신"), ago / 60);
        return string.Format(L("Updated {0}h ago", "{0}시간 전 갱신"), ago / 3600);
    }

    private void QuitAll(object sender, EventArgs e)
    {
        KillBot();
        trayIcon.Visible = false;
        Application.Exit();
    }

    private void RunCmd(string command, bool wait)
    {
        var proc = new Process();
        proc.StartInfo.FileName = "cmd.exe";
        proc.StartInfo.Arguments = "/c " + command;
        proc.StartInfo.UseShellExecute = false;
        proc.StartInfo.CreateNoWindow = true;
        proc.Start();
        if (wait) proc.WaitForExit();
    }

    protected override void OnFormClosing(FormClosingEventArgs e)
    {
        trayIcon.Visible = false;
        base.OnFormClosing(e);
    }

    [STAThread]
    static void Main()
    {
        // Kill any other ClaudeBotTray instances (handles leftover duplicates from updates)
        int myPid = Process.GetCurrentProcess().Id;
        foreach (var proc in Process.GetProcessesByName("ClaudeBotTray"))
        {
            if (proc.Id != myPid)
            {
                try { proc.Kill(); proc.WaitForExit(3000); } catch { }
            }
        }
        Thread.Sleep(500);

        // Single instance check using named Mutex
        bool createdNew;
        using (var mutex = new Mutex(true, "ClaudeBotTray_SingleInstance", out createdNew))
        {
            if (!createdNew)
            {
                // Another instance is already running - exit silently
                return;
            }

            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);
            Application.Run(new ClaudeBotTray());
        }
    }
}
