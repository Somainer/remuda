import type { ButtonHTMLAttributes } from "react";
import ui from "../styles/ui.module.css";
import { CommitProbe } from "./CommitProbe";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "ghost" | "danger" | "default" | "icon" };

const CLASS = {
  default: ui.btn,
  primary: ui.btnPrimary,
  ghost: ui.btnGhost,
  danger: ui.btnDanger,
  icon: ui.iconBtn,
};

/** `icon` buttons carry no text, so they must be named with `aria-label`. */
export function Button({ variant = "default", className, ...props }: Props) {
  return (
    <CommitProbe name="Button">
      <button className={[CLASS[variant], className].filter(Boolean).join(" ")} type="button" {...props} />
    </CommitProbe>
  );
}
