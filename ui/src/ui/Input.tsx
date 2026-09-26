import { forwardRef, type InputHTMLAttributes } from "react";

type Props = InputHTMLAttributes<HTMLInputElement> & { mono?: boolean; big?: boolean };

export const Input = forwardRef<HTMLInputElement, Props>(function Input({ mono, big, className, ...rest }, ref) {
  const cls = ["input", mono ? "mono" : "", big ? "big" : "", className ?? ""].filter(Boolean).join(" ");
  return <input ref={ref} className={cls} spellCheck={false} autoComplete="off" {...rest} />;
});
