export const GITHUB_LATEST_RELEASE_API =
  'https://api.github.com/repos/deepseek-bot/Neteval/releases/latest';
export const GITHUB_RELEASES_PAGE =
  'https://github.com/deepseek-bot/Neteval/releases/latest';
export const AUTO_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000;
export const STARTUP_COOLDOWN_MS = 30 * 60 * 1000;

export interface ReleaseAsset {
  name: string;
  browser_download_url: string;
}

export interface UpdateInfo {
  currentVersion: string;
  latestVersion: string;
  hasUpdate: boolean;
  releaseUrl: string;
  downloadUrl: string;
  assetName: string | null;
  publishedAt: string | null;
  notes: string;
}

export function normalizeVersion(raw: string): string {
  return raw.trim().replace(/^v/i, '');
}

interface ParsedSemver {
  major: number;
  minor: number;
  patch: number;
  pre: string[];
}

function parseSemver(raw: string): ParsedSemver | null {
  const clean = normalizeVersion(raw);
  const noBuild = clean.split('+')[0];
  const dashIdx = noBuild.indexOf('-');
  const core = dashIdx >= 0 ? noBuild.slice(0, dashIdx) : noBuild;
  const prePart = dashIdx >= 0 ? noBuild.slice(dashIdx + 1) : '';
  const parts = core.split('.');
  if (parts.length < 2 || parts.length > 3) return null;
  const nums = parts.map((p) => (/^\d+$/.test(p) ? Number(p) : NaN));
  if (nums.some((n) => !Number.isFinite(n))) return null;
  return {
    major: nums[0],
    minor: nums[1],
    patch: nums[2] ?? 0,
    pre: prePart ? prePart.split('.').filter(Boolean) : [],
  };
}

function comparePreIdentifiers(a: string[], b: string[]): number {
  if (a.length === 0 && b.length === 0) return 0;
  if (a.length === 0) return 1;
  if (b.length === 0) return -1;
  const len = Math.max(a.length, b.length);
  for (let i = 0; i < len; i++) {
    if (i >= a.length) return -1;
    if (i >= b.length) return 1;
    const pa = a[i];
    const pb = b[i];
    const na = /^\d+$/.test(pa) ? Number(pa) : null;
    const nb = /^\d+$/.test(pb) ? Number(pb) : null;
    if (na !== null && nb !== null) {
      if (na !== nb) return na > nb ? 1 : -1;
    } else if (na !== null) {
      return -1;
    } else if (nb !== null) {
      return 1;
    } else if (pa !== pb) {
      return pa > pb ? 1 : -1;
    }
  }
  return 0;
}

export function compareSemver(a: string, b: string): number {
  const pa = parseSemver(a);
  const pb = parseSemver(b);
  if (!pa || !pb) {
    const na = normalizeVersion(a);
    const nb = normalizeVersion(b);
    return na === nb ? 0 : na > nb ? 1 : -1;
  }
  if (pa.major !== pb.major) return pa.major > pb.major ? 1 : -1;
  if (pa.minor !== pb.minor) return pa.minor > pb.minor ? 1 : -1;
  if (pa.patch !== pb.patch) return pa.patch > pb.patch ? 1 : -1;
  return comparePreIdentifiers(pa.pre, pb.pre);
}

export function pickDownloadAsset(
  assets: ReleaseAsset[],
  userAgent: string,
): { downloadUrl: string; assetName: string } | null {
  if (/Android/i.test(userAgent)) {
    const apk = assets.find((a) => /\.apk$/i.test(a.name));
    if (apk) {
      return { downloadUrl: apk.browser_download_url, assetName: apk.name };
    }
    return null;
  }
  if (/Windows/i.test(userAgent)) {
    const setup = assets.find((a) => /-setup\.exe$/i.test(a.name));
    if (setup) {
      return { downloadUrl: setup.browser_download_url, assetName: setup.name };
    }
    const guiExe = assets.find((a) => /^neteval-gui.*\.exe$/i.test(a.name));
    if (guiExe) {
      return { downloadUrl: guiExe.browser_download_url, assetName: guiExe.name };
    }
  }
  return null;
}

interface GithubReleaseResponse {
  tag_name?: string;
  html_url?: string;
  published_at?: string;
  body?: string;
  assets?: Array<{
    name?: string;
    browser_download_url?: string;
  }>;
}

export async function checkLatestRelease(currentVersion: string): Promise<UpdateInfo> {
  const controller = new AbortController();
  const timeoutId = globalThis.setTimeout(() => controller.abort(), 10_000);

  let resp: Response;
  try {
    resp = await fetch(GITHUB_LATEST_RELEASE_API, {
      headers: {
        Accept: 'application/vnd.github+json',
      },
      cache: 'no-store',
      signal: controller.signal,
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === 'AbortError') {
      throw new Error('检查更新超时，请稍后重试');
    }
    throw new Error('无法连接 GitHub 检查更新，请检查网络');
  } finally {
    globalThis.clearTimeout(timeoutId);
  }

  if (!resp.ok) {
    if (resp.status === 403 || resp.status === 429) {
      throw new Error('GitHub API 请求过于频繁，请稍后再试');
    }
    if (resp.status === 404) {
      throw new Error('未找到发布记录');
    }
    throw new Error(`检查更新失败（HTTP ${resp.status}）`);
  }

  const data = (await resp.json()) as GithubReleaseResponse;
  const latestVersion = normalizeVersion(data.tag_name ?? '');
  if (!latestVersion) {
    throw new Error('GitHub 返回的版本号无效');
  }

  const normalizedCurrent = normalizeVersion(currentVersion);
  const releaseUrl = data.html_url || GITHUB_RELEASES_PAGE;
  const assets: ReleaseAsset[] = Array.isArray(data.assets)
    ? data.assets.filter(
        (a): a is ReleaseAsset =>
          typeof a?.name === 'string' && typeof a?.browser_download_url === 'string',
      )
    : [];
  const ua = typeof navigator !== 'undefined' ? navigator.userAgent : '';
  const picked = pickDownloadAsset(assets, ua);

  return {
    currentVersion: normalizedCurrent,
    latestVersion,
    hasUpdate: compareSemver(latestVersion, normalizedCurrent) > 0,
    releaseUrl,
    downloadUrl: picked?.downloadUrl ?? releaseUrl,
    assetName: picked?.assetName ?? null,
    publishedAt: typeof data.published_at === 'string' ? data.published_at : null,
    notes: typeof data.body === 'string' ? data.body : '',
  };
}
