import { ChatInputCommandInteraction, SlashCommandBuilder, EmbedBuilder } from "discord.js";
import { readFileSync, writeFileSync } from "fs";
import { execSync } from "child_process";
import { homedir, platform } from "os";
import { join } from "path";
import { L } from "../../utils/i18n.js";
import { withHostOption } from "./agent-target.js";

export const data = withHostOption(new SlashCommandBuilder()
  .setName("usage")
  .setDescription("Show Claude Code usage (Session 5hr / Weekly / Sonnet)"));

interface UsageEntry {
  utilization: number;
  resets_at: string;
}

interface UsageResponse {
  five_hour?: UsageEntry;
  seven_day?: UsageEntry;
  seven_day_sonnet?: UsageEntry;
  _fetched_at?: string;
}

function progressBar(pct: number, width = 12): string {
  const filled = Math.round((pct / 100) * width);
  return "█".repeat(filled) + "░".repeat(width - filled);
}

function formatResetTime(isoStr: string): string {
  const resetDate = new Date(isoStr);
  const now = new Date();
  const diffMs = resetDate.getTime() - now.getTime();
  if (diffMs <= 0) return L("resetting soon", "곧 초기화");
  const diffH = Math.floor(diffMs / 3600000);
  const diffM = Math.floor((diffMs % 3600000) / 60000);
  if (diffH > 0) return L(`${diffH}h ${diffM}m left`, `${diffH}시간 ${diffM}분 후 초기화`);
  return L(`${diffM}m left`, `${diffM}분 후 초기화`);
}

interface Credentials {
  claudeAiOauth?: {
    accessToken?: string;
    refreshToken?: string;
    expiresAt?: number;
  };
}

function readCredentials(): { cred: Credentials; source: "file" | "keychain" } | null {
  // 1. Try credentials file (Windows/Linux)
  try {
    const credPath = join(homedir(), ".claude", ".credentials.json");
    const cred = JSON.parse(readFileSync(credPath, "utf-8")) as Credentials;
    if (cred?.claudeAiOauth?.accessToken) return { cred, source: "file" };
  } catch { /* not found */ }

  // 2. Try macOS keychain
  if (platform() === "darwin") {
    try {
      const raw = execSync('security find-generic-password -s "Claude Code-credentials" -w 2>/dev/null', {
        encoding: "utf-8",
        timeout: 5000,
      }).trim();
      const cred = JSON.parse(raw) as Credentials;
      if (cred?.claudeAiOauth?.accessToken) return { cred, source: "keychain" };
    } catch { /* keychain not available */ }
  }

  return null;
}

function isTokenExpired(cred: Credentials): boolean {
  const expiresAt = cred?.claudeAiOauth?.expiresAt ?? 0;
  return Date.now() >= expiresAt - 300000;
}

async function refreshOAuthToken(cred: Credentials, source: "file" | "keychain"): Promise<string | null> {
  const refreshToken = cred?.claudeAiOauth?.refreshToken;
  if (!refreshToken) return null;

  try {
    const body = new URLSearchParams({
      grant_type: "refresh_token",
      refresh_token: refreshToken,
      client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
      scope: "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload",
    });

    const res = await fetch("https://platform.claude.com/v1/oauth/token", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: body.toString(),
      signal: AbortSignal.timeout(15000),
    });
    if (!res.ok) return null;

    const data = await res.json() as Record<string, unknown>;
    const newAccess = data.access_token as string;
    if (!newAccess) return null;

    const newRefresh = (data.refresh_token as string) ?? refreshToken;
    const expiresIn = (data.expires_in as number) ?? 3600;
    const newExpiresAt = Date.now() + expiresIn * 1000;

    // Update credentials file (not keychain — avoid popup on macOS)
    if (source === "file") {
      try {
        const credPath = join(homedir(), ".claude", ".credentials.json");
        cred.claudeAiOauth!.accessToken = newAccess;
        cred.claudeAiOauth!.refreshToken = newRefresh;
        cred.claudeAiOauth!.expiresAt = newExpiresAt;
        writeFileSync(credPath, JSON.stringify(cred));
      } catch { /* ignore */ }
    }

    return newAccess;
  } catch {
    return null;
  }
}

async function fetchUsageLive(): Promise<UsageResponse | null> {
  const result = readCredentials();
  if (!result) return null;

  let { cred } = result;
  let token = cred?.claudeAiOauth?.accessToken;
  if (!token) return null;

  // Auto-refresh if expired
  if (isTokenExpired(cred)) {
    const newToken = await refreshOAuthToken(cred, result.source);
    if (newToken) token = newToken;
  }

  try {
    let res = await fetch("https://api.anthropic.com/api/oauth/usage", {
      headers: {
        Authorization: `Bearer ${token}`,
        "anthropic-beta": "oauth-2025-04-20",
      },
      signal: AbortSignal.timeout(10000),
    });

    // 401: retry after refresh
    if (res.status === 401) {
      const newToken = await refreshOAuthToken(cred, result.source);
      if (newToken) {
        res = await fetch("https://api.anthropic.com/api/oauth/usage", {
          headers: {
            Authorization: `Bearer ${newToken}`,
            "anthropic-beta": "oauth-2025-04-20",
          },
          signal: AbortSignal.timeout(10000),
        });
      }
    }

    if (!res.ok) return null;
    const data = (await res.json()) as UsageResponse;

    // Save to cache for tray app and future reads
    try {
      const cachePath = join(homedir(), ".claude", ".usage-cache.json");
      const cache = { ...data, _fetched_at: new Date().toISOString() };
      writeFileSync(cachePath, JSON.stringify(cache));
    } catch { /* ignore cache write failure */ }

    return data;
  } catch {
    return null;
  }
}

function loadUsageCache(): UsageResponse | null {
  try {
    const cachePath = join(homedir(), ".claude", ".usage-cache.json");
    return JSON.parse(readFileSync(cachePath, "utf-8")) as UsageResponse;
  } catch {
    return null;
  }
}

export async function execute(interaction: ChatInputCommandInteraction): Promise<void> {
  // Try live fetch first, fall back to cache
  const data = (await fetchUsageLive()) ?? loadUsageCache();

  if (!data || (!data.five_hour && !data.seven_day && !data.seven_day_sonnet)) {
    await interaction.editReply({
      content: L(
        "Could not fetch usage data. Make sure you're logged into Claude Code (`claude` CLI).",
        "사용량 정보를 가져올 수 없습니다. Claude Code(`claude` CLI)에 로그인되어 있는지 확인하세요."
      ),
    });
    return;
  }

  const lines: string[] = [];

  if (data.five_hour) {
    const pct = Math.round(data.five_hour.utilization);
    lines.push(
      `**${L("Session (5hr)", "세션 (5시간)")}**  \`${progressBar(pct)}\`  **${pct}%**  ·  ${formatResetTime(data.five_hour.resets_at)}`
    );
  }
  if (data.seven_day) {
    const pct = Math.round(data.seven_day.utilization);
    lines.push(
      `**${L("Weekly (7day)", "주간 (7일)")}**  \`${progressBar(pct)}\`  **${pct}%**  ·  ${formatResetTime(data.seven_day.resets_at)}`
    );
  }
  if (data.seven_day_sonnet) {
    const pct = Math.round(data.seven_day_sonnet.utilization);
    lines.push(
      `**${L("Sonnet (7day)", "소네트 (7일)")}**  \`${progressBar(pct)}\`  **${pct}%**  ·  ${formatResetTime(data.seven_day_sonnet.resets_at)}`
    );
  }

  // Show last fetched time
  let footerText = L("claude.ai/settings/usage", "claude.ai/settings/usage");
  if (data._fetched_at) {
    const fetchedDate = new Date(data._fetched_at);
    const diffMin = Math.floor((Date.now() - fetchedDate.getTime()) / 60000);
    if (diffMin < 1) {
      footerText = L("Just now", "방금 갱신") + "  ·  " + footerText;
    } else {
      footerText = L(`${diffMin}m ago`, `${diffMin}분 전 갱신`) + "  ·  " + footerText;
    }
  }

  const embed = new EmbedBuilder()
    .setTitle(L("📊 Claude Code Usage", "📊 Claude Code 사용량"))
    .setDescription(lines.join("\n\n"))
    .setColor(0x7c3aed)
    .setFooter({ text: footerText })
    .setTimestamp();

  await interaction.editReply({ embeds: [embed] });
}
