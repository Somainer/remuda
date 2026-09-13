/** Build a cwd from a selected Node workspace without accepting a different root. */
export function workspaceCwd(root: string, subpath: string): string | null {
  const relative = subpath.trim();
  if (/^(?:[/\\]|[A-Za-z]:|~|\$HOME(?:[/\\]|$))/.test(relative)) return null;
  const parts: string[] = [];
  for (const part of relative.split(/[\\/]/)) {
    if (!part || part === ".") continue;
    if (part === "..") {
      if (!parts.length) return null;
      parts.pop();
    } else parts.push(part);
  }
  return [root.replace(/\/$/, ""), ...parts].join("/") || "/";
}
