import ui from "../../styles/ui.module.css";
import css from "./hosts.module.css";

export type HostBindingKind = "auto" | "native" | "profile";

export type BindingProfile = { id: string; name: string; scope: string };

export function parseProviderBinding(raw: string | undefined): { kind: HostBindingKind; profileId: string } {
  const value = (raw ?? "auto").trim();
  if (value === "native") return { kind: "native", profileId: "" };
  if (value.startsWith("profile:")) return { kind: "profile", profileId: value.slice("profile:".length) };
  return { kind: "auto", profileId: "" };
}

export function formatProviderBinding(kind: HostBindingKind, profileId: string): string {
  if (kind === "native") return "native";
  if (kind === "profile" && profileId) return `profile:${profileId}`;
  return "auto";
}

type Props = {
  binding: string;
  profiles: BindingProfile[];
  disabled?: boolean;
  onChange: (binding: string) => void;
};

export function HostProviderBinding({ binding, profiles, disabled, onChange }: Props) {
  const parsed = parseProviderBinding(binding);
  return (
    <div className={ui.field} data-testid="host-provider-binding">
      <span>Provider</span>
      <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
        {(
          [
            ["auto", "自动"],
            ["native", "原生登录"],
            ["profile", "指定 provider"],
          ] as const
        ).map(([id, label]) => (
          <button
            key={id}
            type="button"
            className={parsed.kind === id ? css.add : css.toggle}
            data-testid={`host-binding-${id}`}
            disabled={disabled}
            onClick={() => {
              if (id === "profile") {
                onChange(formatProviderBinding("profile", parsed.profileId || profiles[0]?.id || ""));
                return;
              }
              onChange(id);
            }}
          >
            {label}
          </button>
        ))}
      </div>
      {parsed.kind === "profile" ? (
        <select
          className={ui.input}
          data-testid="host-binding-profile-id"
          disabled={disabled}
          value={parsed.profileId}
          onChange={(e) => onChange(formatProviderBinding("profile", e.target.value))}
        >
          {profiles.map((profile) => (
            <option key={profile.id} value={profile.id}>
              {profile.name} ({profile.scope.startsWith("host:") ? "host" : "universal"})
            </option>
          ))}
        </select>
      ) : null}
    </div>
  );
}
