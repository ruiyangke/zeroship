import type { ComponentPropsWithoutRef } from "react";

export function MemberList(props: Omit<ComponentPropsWithoutRef<"ul">, "className">) {
  return <ul {...props} className="member-list mt-2 grid list-none gap-1 p-0" />;
}

export function MemberListItem(props: Omit<ComponentPropsWithoutRef<"li">, "className">) {
  return (
    <li
      {...props}
      className="flex max-w-88 items-center justify-between gap-3"
    />
  );
}
