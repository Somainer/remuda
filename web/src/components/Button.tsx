import type { ButtonHTMLAttributes } from "react";
import ui from "../styles/ui.module.css";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & { variant?: "primary" | "ghost" | "danger" | "default" };

export function Button({ variant = "default", className, ...props }: Props) {
  const map = {
    default: ui.btn,
    primary: ui.btnPrimary,
    ghost: ui.btnGhost,
    danger: ui.btnDanger,
  };
  return <button className={[map[variant], className].filter(Boolean).join(" ")} type="button" {...props} />;
}
