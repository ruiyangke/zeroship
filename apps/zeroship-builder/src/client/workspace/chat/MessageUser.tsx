import type { ReactNode } from "react";

export interface MessageUserProps {
  text: string;
  time?: string;
  attachments?: ReactNode;
}

export function MessageUser({ text, time, attachments }: MessageUserProps) {
  return (
    <div data-testid="msg-user">
      <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-1">
        You{time ? ` · ${time}` : ""}
      </div>
      <div className="font-serif text-[15px] leading-snug text-ink border-l-2 border-tomato pl-3.5 whitespace-pre-wrap break-words">
        {text}
      </div>
      {attachments && <div className="mt-1 pl-3.5">{attachments}</div>}
    </div>
  );
}
