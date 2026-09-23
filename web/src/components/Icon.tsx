import type { LucideIcon, LucideProps } from "lucide-react";

/**
 * The one icon wrapper (ui-spec: lucide-react, no second set). 16px glyph at a
 * 1.75 stroke by default; decorative unless given a `label`, in which case it
 * is announced as an image.
 */
export function Icon({
  icon: Glyph,
  size = 16,
  label,
  ...props
}: Omit<LucideProps, "ref"> & { icon: LucideIcon; label?: string }) {
  return (
    <Glyph
      size={size}
      strokeWidth={1.75}
      aria-hidden={label ? undefined : true}
      aria-label={label}
      role={label ? "img" : undefined}
      focusable={false}
      {...props}
    />
  );
}
