// Minimal "find a user" widget: type a fragment of name/handle/email, pick
// from the matches. There is no dedicated typeahead RPC, so the submitted text
// becomes the argument to the cached users.list({text}) query -- which is
// exactly what it is for.
import { useState } from "react";
import { Button, Input } from "@zeroship/ui";
import { useUserSearch } from "../lib/queries";
import { errorMessage } from "./rpc";
import type { UserRow } from "./types";

export function UserPicker({
  placeholder = "name, handle, or email",
  onPick,
}: {
  placeholder?: string;
  onPick: (user: UserRow) => void;
}) {
  const [text, setText] = useState("");
  const [submittedText, setSubmittedText] = useState<string | null>(null);
  const usersQ = useUserSearch(
    { text: submittedText ?? "", limit: 8 },
    { enabled: Boolean(submittedText) },
  );
  const results = submittedText && !usersQ.isError ? usersQ.data ?? null : null;
  const error = submittedText && usersQ.isError && !usersQ.isFetching
    ? errorMessage(usersQ.error)
    : null;

  const search = () => {
    const query = text.trim();
    if (!query) {
      setSubmittedText(null);
      return;
    }

    // The Find button remains an explicit action rather than turning this
    // into a typeahead. Re-submitting the same key asks the query to refetch;
    // changing the submitted argument starts the newly enabled query.
    if (query === submittedText) {
      void usersQ.refetch();
    } else {
      setSubmittedText(query);
    }
  };

  return (
    <div className="user-picker">
      <div className="user-picker-row">
        <Input
          value={text}
          placeholder={placeholder}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              search();
            }
          }}
        />
        <Button variant="gray" size="sm" onClick={search} disabled={usersQ.isFetching}>
          {usersQ.isFetching ? "..." : "Find"}
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      {results ? (
        results.length === 0 ? (
          <p className="state-hint small">No matching users.</p>
        ) : (
          <ul className="user-picker-results">
            {results.map((user) => (
              <li key={user.id}>
                <Button variant="gray" size="sm"
                  onClick={() => {
                    onPick(user);
                    setText("");
                    setSubmittedText(null);
                  }}
                >
                  {user.name} <span className="dim">@{user.handle}</span>
                </Button>
              </li>
            ))}
          </ul>
        )
      ) : null}
    </div>
  );
}
