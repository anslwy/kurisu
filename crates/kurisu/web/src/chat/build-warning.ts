export async function checkBuild(bundle: string, git_sha: string): Promise<string> {
  try {
    const response = await fetch(`/_web/api/build?${new URLSearchParams({bundle, git_sha})}`, {cache: "no-store"});
    if (!response.ok) return "";
    const check = await response.json();
    return check.status === "FAIL" ? "This page is from a different build than the running server. Reload this page." : "";
  } catch { return ""; }
}
