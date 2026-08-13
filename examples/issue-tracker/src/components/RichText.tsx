import { Button, Cluster } from "@zeroship/ui";
import { EditorContent, useEditor, type Editor } from "@tiptap/react";
import StarterKit from "@tiptap/starter-kit";
import { useEffect } from "react";

/**
 * Rich text for bug descriptions and comments, on tiptap.
 *
 * WHY RENDERING GOES THROUGH TIPTAP TOO, rather than dangerouslySetInnerHTML.
 *
 * The stored value is HTML, and it is user input: `comments.add` takes a
 * string over RPC, so a caller can send whatever they like without ever
 * touching this editor. Rendering that with `dangerouslySetInnerHTML` is
 * stored XSS -- one crafted comment and every reader of the bug runs it.
 *
 * A read-only editor parses the HTML through the SAME schema that produced it.
 * A node or attribute the schema does not define is dropped rather than
 * rendered, so `<script>`, `onerror=` and `javascript:` hrefs cannot survive
 * the trip. The sanitiser and the writer are the same object, which is the
 * only version of this that cannot drift apart.
 *
 * The starter kit's node set is the whole vocabulary: paragraphs, headings,
 * lists, code, blockquote, emphasis. Adding a node means widening what is
 * rendered, so it is a security decision, not a formatting one.
 */

const EXTENSIONS = [StarterKit];

/**
 * Does this document contain anything?
 *
 * An empty tiptap document serialises to "<p></p>", so the usual
 * `value.trim()` guard sees a non-empty string and lets a blank comment
 * through. Strip the tags and ask about the text.
 */
export function hasText(html: string): boolean {
  return html.replace(/<[^>]*>/g, "").replace(/&nbsp;/g, " ").trim().length > 0;
}

export function RichText({ html }: { html: string }) {
  const editor = useEditor(
    {
      extensions: EXTENSIONS,
      content: html,
      editable: false,
      // Tiptap warns without this when the same content renders on a server
      // and then hydrates; the app is client-only but the flag is free.
      immediatelyRender: false,
    },
    [html],
  );
  if (!editor) return null;
  return <EditorContent editor={editor} className="rich-text" />;
}

function ToolbarButton({
  editor,
  label,
  isActive,
  onClick,
}: {
  editor: Editor;
  label: string;
  isActive: boolean;
  onClick: () => void;
}) {
  return (
    <Button
      variant={isActive ? "tinted" : "plain"}
      size="small"
      aria-pressed={isActive}
      // The editor keeps focus, so a click on the toolbar formats the
      // selection instead of clearing it.
      onMouseDown={(event) => event.preventDefault()}
      onClick={onClick}
      disabled={!editor.isEditable}
    >
      {label}
    </Button>
  );
}

export function RichTextEditor({
  value,
  onChange,
  placeholder,
  ariaLabel,
}: {
  value: string;
  onChange: (html: string) => void;
  placeholder?: string;
  ariaLabel: string;
}) {
  const editor = useEditor({
    extensions: EXTENSIONS,
    content: value,
    immediatelyRender: false,
    editorProps: {
      attributes: {
        class: "rich-text-input",
        "aria-label": ariaLabel,
        ...(placeholder ? { "data-placeholder": placeholder } : {}),
      },
    },
    onUpdate: ({ editor: next }) => onChange(next.getHTML()),
  });

  // Reset when the caller clears the field -- posting a comment empties the
  // box, and without this the editor keeps the text it just submitted.
  useEffect(() => {
    if (!editor) return;
    if (value === "" && editor.getText() !== "") editor.commands.clearContent();
  }, [editor, value]);

  if (!editor) return null;

  return (
    <div className="rich-text-editor">
      <Cluster gap={1} className="rich-text-toolbar">
        <ToolbarButton
          editor={editor}
          label="B"
          isActive={editor.isActive("bold")}
          onClick={() => editor.chain().focus().toggleBold().run()}
        />
        <ToolbarButton
          editor={editor}
          label="I"
          isActive={editor.isActive("italic")}
          onClick={() => editor.chain().focus().toggleItalic().run()}
        />
        <ToolbarButton
          editor={editor}
          label="Code"
          isActive={editor.isActive("code")}
          onClick={() => editor.chain().focus().toggleCode().run()}
        />
        <ToolbarButton
          editor={editor}
          label="List"
          isActive={editor.isActive("bulletList")}
          onClick={() => editor.chain().focus().toggleBulletList().run()}
        />
        <ToolbarButton
          editor={editor}
          label="1."
          isActive={editor.isActive("orderedList")}
          onClick={() => editor.chain().focus().toggleOrderedList().run()}
        />
        <ToolbarButton
          editor={editor}
          label="Quote"
          isActive={editor.isActive("blockquote")}
          onClick={() => editor.chain().focus().toggleBlockquote().run()}
        />
        <ToolbarButton
          editor={editor}
          label="{ }"
          isActive={editor.isActive("codeBlock")}
          onClick={() => editor.chain().focus().toggleCodeBlock().run()}
        />
      </Cluster>
      <EditorContent editor={editor} />
    </div>
  );
}
