import { Button, Cluster, Input } from "@zeroship/ui";
import { EditorContent, useEditor, type Editor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { Markdown } from "@tiptap/markdown";
import { Placeholder } from "@tiptap/extensions";
import { useEffect, useState } from "react";

import { AppFieldShell } from "./AppFieldShell";
import { FieldError } from "./AppPrimitives";

/**
 * Rich text for issue descriptions and comments, on tiptap.
 *
 * WHY RENDERING GOES THROUGH TIPTAP TOO, rather than dangerouslySetInnerHTML.
 *
 * The stored value is user input: `comments.add` takes a body string over
 * RPC, so a caller can send whatever they like -- markdown, embedded HTML,
 * anything -- without ever touching this editor. Rendering that with
 * `dangerouslySetInnerHTML` is stored XSS: one crafted comment and every
 * reader of the issue runs it.
 *
 * A read-only editor parses the HTML through the SAME schema that produced it,
 * so the sanitiser and the writer are the same object -- the only version of
 * this that cannot drift apart.
 *
 * It takes TWO mechanisms, not one. This comment used to credit everything to
 * the schema and omit the node that actually carries a URL:
 *
 *   1. Unknown tags and attributes. `<script>`, `<iframe>`, `<img>` and every
 *      `on*` handler are absent from the schema, so ProseMirror's DOM parser
 *      drops them. Nothing about hrefs here.
 *   2. Link hrefs. StarterKit 3.30 DOES register Link (along with Heading and
 *      UndoRedo), so `<a href>` is part of the vocabulary. What makes it safe
 *      is the extension's own protocol allowlist -- http, https, ftp, ftps,
 *      mailto, tel, callto, sms, cid, xmpp -- enforced in `parseHTML`, where
 *      a failing href returns false and the mark never parses at all, and
 *      again in `renderHTML`. `javascript:` is not on the list, and unicode
 *      whitespace is stripped before matching, so `java\tscript:` does not
 *      slip past. Verified in the installed extension, not assumed.
 *
 * Both arms are driven by e2e/comment-xss.spec.ts, which stores hostile markup
 * through the real `comments.add` RPC and asserts nothing executes. Swap this
 * component back to dangerouslySetInnerHTML and that spec goes red on a payload
 * that really fires -- checked, so the defence is known to be load-bearing
 * rather than merely present.
 *
 * Widening EXTENSIONS widens what renders, so adding a node is a security
 * decision, not a formatting one -- and the spec pins payloads, not the
 * schema, so it will not notice on your behalf.
 */

/**
 * Bodies are stored as MARKDOWN, not HTML.
 *
 * An issue tracker's comments end up in more places than the page that wrote
 * them: notification emails, the database when someone greps it, a CLI or an
 * agent filing an issue over `comments.add`. Markdown is legible in all of those;
 * a wall of serialised HTML is legible in none.
 *
 * This does NOT make the XSS story go away, and it is worth being explicit
 * because the opposite is the natural assumption. Markdown permits embedded
 * raw HTML by spec, and the Markdown extension deliberately parses it through
 * the same `parseHTML` rules as everything else. The schema is still the only
 * thing standing between a stored payload and every reader -- storing markdown
 * narrows what we persist, it does not sanitise anything.
 *
 * Stored HTML from before this change still renders, because embedded HTML is
 * exactly what the markdown parser handles.
 */
const EXTENSIONS = [StarterKit, Markdown];

/**
 * Writing extensions: the vocabulary above, plus a placeholder.
 *
 * The placeholder was styled but never rendered. The CSS read
 * `content: attr(data-placeholder)` from the empty paragraph while the
 * attribute sat on the editor root, so attr() resolved to nothing -- and the
 * `is-editor-empty` class it hangs on comes from an extension that was never
 * registered. It went unnoticed while the toolbar was always visible: twelve
 * buttons say "this is an editor" loudly enough that nobody missed the hint.
 * Collapsing the toolbar left a bare rounded box with no clue what it was.
 *
 * Read-only rendering deliberately does NOT get this. A comment with no text
 * is not an invitation to write one.
 */
const WRITING_EXTENSIONS = [
  ...EXTENSIONS,
  Placeholder.configure({ placeholder: "Add a comment..." }),
];

/**
 * Does this document contain anything?
 *
 * An empty document does not serialise to "": as HTML it was "<p></p>", and
 * as markdown it can still be whitespace or a stray newline. A bare
 * `value.trim()` guard let a blank comment through on the HTML form and the
 * submit button disagreed with the submit handler about it. Strip any markup
 * and ask about the remaining text, so the answer does not depend on which
 * format the body is in.
 */
export function hasText(body: string): boolean {
  return body.replace(/<[^>]*>/g, "").replace(/&nbsp;/g, " ").trim().length > 0;
}

export function RichText({ markdown }: { markdown: string }) {
  const editor = useEditor(
    {
      extensions: EXTENSIONS,
      content: markdown,
      contentType: "markdown",
      editable: false,
      // Tiptap warns without this when the same content renders on a server
      // and then hydrates; the app is client-only but the flag is free.
      immediatelyRender: false,
    },
    [markdown],
  );
  if (!editor) return null;
  return <RichTextContent editor={editor} className="rich-text" />;
}

/** Typography shared by the read-only renderer and the writing surface. */
function RichTextContent({ editor, className = "" }: { editor: Editor; className?: string }) {
  return (
    <EditorContent
      editor={editor}
      className={`[&_p:first-child]:mt-0 [&_p:last-child]:mb-0 [&_pre]:overflow-x-auto [&_pre]:rounded-lg [&_pre]:bg-surface-sunken [&_pre]:px-3 [&_pre]:py-2 [&_blockquote]:mx-0 [&_blockquote]:border-s-[3px] [&_blockquote]:border-line-strong [&_blockquote]:ps-3 [&_blockquote]:text-ink-secondary ${className}`}
    />
  );
}

function ToolbarButton({
  editor,
  label,
  title,
  isActive,
  disabled,
  onClick,
}: {
  editor: Editor;
  label: string;
  /** Accessible name when the visible label is a glyph like "B" or "1.". */
  title?: string;
  isActive: boolean;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <Button
      variant={isActive ? "tinted" : "plain"}
      size="sm"
      aria-pressed={isActive}
      aria-label={title}
      title={title}
      // The editor keeps focus, so a click on the toolbar formats the
      // selection instead of clearing it.
      onMouseDown={(event) => event.preventDefault()}
      onClick={onClick}
      disabled={!editor.isEditable || disabled}
    >
      {label}
    </Button>
  );
}

/**
 * The link control.
 *
 * Separate from the plain toggles because a link needs a value, and because
 * that value can be REFUSED: `setLink` runs the same protocol allowlist that
 * guards rendering, so `javascript:...` silently fails to apply. Silently is
 * the problem -- a toolbar that accepts your input, closes, and produces no
 * link reads as a broken button rather than a rejected URL. So the refusal is
 * detected (the mark is absent afterwards) and stated.
 */
function LinkControl({ editor }: { editor: Editor }) {
  const [open, setOpen] = useState(false);
  const [href, setHref] = useState("");
  const [refused, setRefused] = useState(false);
  const active = editor.isActive("link");

  const apply = () => {
    const url = href.trim();
    if (!url) return;
    editor.chain().focus().extendMarkRange("link").setLink({ href: url }).run();
    // Ask the document, not the return value: the command reports whether it
    // dispatched, not whether the href passed validation.
    if (editor.isActive("link")) {
      setOpen(false);
      setHref("");
      setRefused(false);
    } else {
      setRefused(true);
    }
  };

  return (
    <>
      <ToolbarButton
        editor={editor}
        label="Link"
        isActive={active}
        onClick={() => {
          if (active) {
            editor.chain().focus().extendMarkRange("link").unsetLink().run();
            return;
          }
          setRefused(false);
          setHref("");
          setOpen((v) => !v);
        }}
      />
      {open ? (
        <Cluster gap={1} align="center">
          <Input
            aria-label="Link URL"
            placeholder="https://example.com"
            value={href}
            onChange={(event) => setHref(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                apply();
              }
              if (event.key === "Escape") setOpen(false);
            }}
          />
          <Button variant="filled" size="sm" onMouseDown={(e) => e.preventDefault()} onClick={apply}>
            Apply
          </Button>
          {refused ? (
            <FieldError as="span" role="alert">
              That link was refused. Use http, https or mailto.
            </FieldError>
          ) : null}
        </Cluster>
      ) : null}
    </>
  );
}

export function RichTextEditor({
  value,
  onChange,
  placeholder,
  ariaLabel,
  collapsible = false,
}: {
  value: string;
  onChange: (html: string) => void;
  placeholder?: string;
  ariaLabel: string;
  /**
   * Keep the toolbar out of the way until there is something to format.
   *
   * The comment composer sits at the foot of every issue page, so twelve
   * formatting buttons were permanently on screen under the thread -- more
   * chrome than the empty box they belonged to, and all of it addressing a
   * task nobody had started. Editing an existing comment does NOT set this:
   * there the toolbar is the point of having opened the editor.
   */
  collapsible?: boolean;
}) {
  const [focused, setFocused] = useState(false);
  const editor = useEditor({
    extensions: WRITING_EXTENSIONS,
    content: value,
    immediatelyRender: false,
    editorProps: {
      attributes: {
        class:
          "rich-text-input min-h-24 px-3 py-2 outline-none empty:before:pointer-events-none empty:before:float-start empty:before:h-0 empty:before:text-ink-muted empty:before:content-[attr(data-placeholder)] [&_p.is-editor-empty:first-child]:before:pointer-events-none [&_p.is-editor-empty:first-child]:before:float-start [&_p.is-editor-empty:first-child]:before:h-0 [&_p.is-editor-empty:first-child]:before:text-ink-muted [&_p.is-editor-empty:first-child]:before:content-[attr(data-placeholder)]",
        "aria-label": ariaLabel,
        ...(placeholder ? { "data-placeholder": placeholder } : {}),
      },
    },
    onUpdate: ({ editor: next }) => onChange(next.getMarkdown()),
    onFocus: () => setFocused(true),
    onBlur: () => setFocused(false),
  });

  // Reset when the caller clears the field -- posting a comment empties the
  // box, and without this the editor keeps the text it just submitted.
  useEffect(() => {
    if (!editor) return;
    if (value === "" && editor.getText() !== "") editor.commands.clearContent();
  }, [editor, value]);

  if (!editor) return null;

  return (
    <AppFieldShell className="rich-text-editor overflow-hidden">
      {/* Shown once the editor has focus or content. A blurred, empty
          composer needs no formatting controls. */}
      {!collapsible || focused || hasText(value) ? (
      <Cluster
        gap={1}
        className="border-b border-line bg-surface-sunken px-1 py-1"
      >
        <ToolbarButton
          editor={editor}
          label="B"
          title="Bold"
          isActive={editor.isActive("bold")}
          onClick={() => editor.chain().focus().toggleBold().run()}
        />
        <ToolbarButton
          editor={editor}
          label="I"
          title="Italic"
          isActive={editor.isActive("italic")}
          onClick={() => editor.chain().focus().toggleItalic().run()}
        />
        <ToolbarButton
          editor={editor}
          label="Code"
          title="Inline code"
          isActive={editor.isActive("code")}
          onClick={() => editor.chain().focus().toggleCode().run()}
        />
        <ToolbarButton
          editor={editor}
          label="List"
          title="Bullet list"
          isActive={editor.isActive("bulletList")}
          onClick={() => editor.chain().focus().toggleBulletList().run()}
        />
        <ToolbarButton
          editor={editor}
          label="1."
          title="Numbered list"
          isActive={editor.isActive("orderedList")}
          onClick={() => editor.chain().focus().toggleOrderedList().run()}
        />
        <ToolbarButton
          editor={editor}
          label="Quote"
          title="Blockquote"
          isActive={editor.isActive("blockquote")}
          onClick={() => editor.chain().focus().toggleBlockquote().run()}
        />
        <ToolbarButton
          editor={editor}
          label="{ }"
          title="Code block"
          isActive={editor.isActive("codeBlock")}
          onClick={() => editor.chain().focus().toggleCodeBlock().run()}
        />
        {/* Headings and undo/redo were missing from the toolbar while being
            registered in the schema all along -- StarterKit 3.30 ships Heading
            and UndoRedo, so the editor already accepted both by keyboard and
            by paste. These buttons expose what was reachable, they do not
            widen what the document can hold. */}
        <ToolbarButton
          editor={editor}
          label="H2"
          title="Heading"
          isActive={editor.isActive("heading", { level: 2 })}
          onClick={() => editor.chain().focus().toggleHeading({ level: 2 }).run()}
        />
        <ToolbarButton
          editor={editor}
          label="H3"
          title="Subheading"
          isActive={editor.isActive("heading", { level: 3 })}
          onClick={() => editor.chain().focus().toggleHeading({ level: 3 }).run()}
        />
        <LinkControl editor={editor} />
        <ToolbarButton
          editor={editor}
          label="Undo"
          isActive={false}
          disabled={!editor.can().undo()}
          onClick={() => editor.chain().focus().undo().run()}
        />
        <ToolbarButton
          editor={editor}
          label="Redo"
          isActive={false}
          disabled={!editor.can().redo()}
          onClick={() => editor.chain().focus().redo().run()}
        />
      </Cluster>
      ) : null}
      <RichTextContent editor={editor} />
    </AppFieldShell>
  );
}
