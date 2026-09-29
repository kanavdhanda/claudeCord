// Validation for the "clone a public GitHub repo" flow. Never builds a shell string.

export interface RepoRef {
  owner: string;
  repo: string;
  cloneUrl: string;
  dirName: string;
}

const NAME = /^[A-Za-z0-9._-]+$/;

export function parseGithubRepo(input: string): RepoRef | null {
  const trimmed = input.trim().replace(/\/+$/, "");
  const short = trimmed.match(/^([A-Za-z0-9._-]+)\/([A-Za-z0-9._-]+)$/);
  const long = trimmed.match(/^https:\/\/github\.com\/([A-Za-z0-9._-]+)\/([A-Za-z0-9._-]+?)(?:\.git)?$/);
  const m = long ?? short;
  if (!m) return null;
  const owner = m[1];
  const repo = m[2].replace(/\.git$/, "");
  if (!NAME.test(owner) || !NAME.test(repo) || repo === "." || repo === ".." || owner.startsWith("-")) return null;
  return { owner, repo, cloneUrl: `https://github.com/${owner}/${repo}.git`, dirName: repo };
}

/** Discord channel names: lowercase, 1-100 chars, dashes only. */
export function channelNameFor(host: string, repo: string): string {
  return `${host}-${repo}`
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 100);
}
