import type { ButtonHTMLAttributes } from "react";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & { kind?: "primary" | "quiet" | "plain"; small?: boolean };

// primary: the grass .btn.or. quiet: hairline. plain: text only.
export function Button({ kind = "quiet", small, className, type = "button", ...rest }: Props) {
  const cls = ["btn", kind, small ? "sm" : "", className ?? ""].filter(Boolean).join(" ");
  return <button type={type} className={cls} {...rest} />;
}
