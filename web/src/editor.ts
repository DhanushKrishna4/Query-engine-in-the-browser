/**
 * The SQL editor: CodeMirror 6, with the three things that make it worth
 * having over a `<textarea>`.
 *
 * **Syntax highlighting** comes from `@codemirror/lang-sql`, which also drives
 * the other two -- it knows where an identifier is expected and where a string
 * literal is.
 *
 * **Schema-aware completion** is fed from the engine's own catalog, so the
 * table and column names offered are the ones actually loaded. A completion
 * list built from a hard-coded schema is worse than none: it is confidently
 * wrong the moment a dataset changes.
 *
 * **Inline error markers** come from the parser's byte offsets. Every token,
 * AST node and bound expression in this engine carries a span, which was built
 * in from the first commit for exactly this -- so an error can underline the
 * characters that caused it rather than naming a line number.
 *
 * ## Byte offsets are not character offsets
 *
 * The engine counts bytes; CodeMirror counts UTF-16 code units. For ASCII they
 * are the same number, which is why this is easy to get wrong and never notice.
 * `byteToChar` below does the conversion, and a query containing a non-ASCII
 * string literal is what makes it matter.
 */
import { autocompletion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { sql, SQLite } from "@codemirror/lang-sql";
import { linter, type Diagnostic, lintGutter } from "@codemirror/lint";
import { Compartment, EditorState, StateEffect, StateField } from "@codemirror/state";
import { Decoration, EditorView, keymap, placeholder, type DecorationSet } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { defaultKeymap } from "@codemirror/commands";

/** A table as the catalog describes it. */
export interface TableInfo {
  name: string;
  columns: { name: string; type: string }[];
}

/** Where an engine diagnostic points, in bytes. */
export interface SpanError {
  message: string;
  start: number;
  end: number;
}

/**
 * Convert a byte offset into the character offset CodeMirror wants.
 *
 * Walks the string once, counting UTF-8 bytes per code point. Cheap for the
 * length of query anyone types, and correct for the ones with an accent in a
 * string literal -- where treating bytes as characters silently shifts the
 * underline.
 */
export function byteToChar(text: string, byte: number): number {
  if (byte <= 0) return 0;
  let bytes = 0;
  for (let i = 0; i < text.length; ) {
    if (bytes >= byte) return i;
    const code = text.codePointAt(i)!;
    bytes += code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
    i += code >= 0x10000 ? 2 : 1;
  }
  return text.length;
}

export interface EditorOptions {
  parent: HTMLElement;
  initial: string;
  onRun: () => void;
  /** Re-parsed on every change; return null when the query is valid. */
  check: (sql: string) => SpanError | null;
  /** The catalog, for completion. Read afresh each time it is consulted. */
  tables: () => TableInfo[];
}

/**
 * A transient highlight over a byte range, for the tokens panel.
 *
 * A `StateEffect` rather than a direct DOM change because CodeMirror owns the
 * document's rendering: anything drawn behind its back is wiped by the next
 * edit or scroll.
 */
const setHighlight = StateEffect.define<{ from: number; to: number } | null>();

const highlightMark = Decoration.mark({ class: "cm-token-highlight" });

const highlightField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(marks, tr) {
    for (const effect of tr.effects) {
      if (effect.is(setHighlight)) {
        return effect.value === null
          ? Decoration.none
          : Decoration.set([highlightMark.range(effect.value.from, effect.value.to)]);
      }
    }
    // A highlight points at a range of the document it was computed from, so
    // an edit invalidates it rather than moving it.
    return tr.docChanged ? Decoration.none : marks;
  },
  provide: (field) => EditorView.decorations.from(field),
});

export class SqlEditor {
  readonly view: EditorView;
  private readonly schema = new Compartment();

  constructor(private readonly options: EditorOptions) {
    this.view = new EditorView({
      parent: options.parent,
      state: EditorState.create({
        doc: options.initial,
        extensions: [
          basicSetup,
          this.schema.of(this.sqlExtension()),
          autocompletion({ override: [(c) => this.complete(c)] }),
          linter((view) => this.lint(view), { delay: 250 }),
          lintGutter(),
          highlightField,
          placeholder("SELECT … FROM …"),
          EditorView.lineWrapping,
          keymap.of([
            {
              // The engine takes a moment on a large query, so running is
              // explicit rather than on every keystroke.
              key: "Mod-Enter",
              preventDefault: true,
              run: () => {
                options.onRun();
                return true;
              },
            },
            ...defaultKeymap,
          ]),
          theme,
        ],
      }),
    });
  }

  get value(): string {
    return this.view.state.doc.toString();
  }

  set value(text: string) {
    this.view.dispatch({
      changes: { from: 0, to: this.view.state.doc.length, insert: text },
    });
  }

  focus() {
    this.view.focus();
  }

  /**
   * Underline a byte range of the source, or clear it with nulls.
   *
   * Byte offsets, converted here, because everything the engine reports is in
   * bytes and the caller should not have to remember that twice.
   */
  highlight(start: number | null, end: number | null) {
    const text = this.view.state.doc.toString();
    const range =
      start === null || end === null
        ? null
        : {
            from: Math.min(byteToChar(text, start), text.length),
            to: Math.min(byteToChar(text, end), text.length),
          };
    this.view.dispatch({ effects: setHighlight.of(range && range.to > range.from ? range : null) });
  }

  /** Re-read the catalog after a dataset loads, so completion stays truthful. */
  refreshSchema() {
    this.view.dispatch({
      effects: this.schema.reconfigure(this.sqlExtension()),
    });
  }

  private sqlExtension() {
    const schema: Record<string, string[]> = {};
    for (const table of this.options.tables()) {
      schema[table.name] = table.columns.map((c) => c.name);
    }
    return sql({ dialect: SQLite, schema, upperCaseKeywords: true });
  }

  /**
   * Completion over the live catalog.
   *
   * `lang-sql` already completes qualified names from the schema above; this
   * adds the bare table and column names and annotates each with its type, so
   * the list says what a column *is* and not merely that it exists.
   */
  private complete(context: CompletionContext): CompletionResult | null {
    const word = context.matchBefore(/[\w.]*/);
    if (!word || (word.from === word.to && !context.explicit)) return null;

    const tables = this.options.tables();
    const options = [
      ...tables.map((t) => ({
        label: t.name,
        type: "class",
        detail: `${t.columns.length} columns`,
        boost: 1,
      })),
      ...tables.flatMap((t) =>
        t.columns.map((c) => ({
          label: c.name,
          type: "property",
          detail: `${t.name} · ${c.type.toLowerCase()}`,
        }))
      ),
    ];
    return { from: word.from, options, validFor: /^[\w.]*$/ };
  }

  /**
   * Underline the characters the parser or binder objected to.
   *
   * An empty document is not an error -- reporting one before anything has
   * been typed is noise -- and a diagnostic with no span underlines the whole
   * query rather than guessing at a position.
   */
  private lint(view: EditorView): Diagnostic[] {
    const text = view.state.doc.toString();
    if (text.trim() === "") return [];

    const error = this.options.check(text);
    if (!error) return [];

    const from = byteToChar(text, error.start);
    const to = Math.max(from + 1, byteToChar(text, error.end));
    return [
      {
        from: Math.min(from, text.length),
        to: Math.min(to, text.length),
        severity: "error",
        message: error.message,
      },
    ];
  }
}

/** Matches the page's palette rather than CodeMirror's default light theme. */
const theme = EditorView.theme(
  {
    "&": { backgroundColor: "transparent", color: "var(--fg)", fontSize: "0.85rem" },
    ".cm-content": { fontFamily: "var(--mono)", caretColor: "var(--accent)" },
    ".cm-gutters": {
      backgroundColor: "transparent",
      color: "var(--dim)",
      border: "none",
    },
    ".cm-activeLine": { backgroundColor: "rgba(255,255,255,.03)" },
    ".cm-token-highlight": {
      backgroundColor: "var(--accent-dim)",
      outline: "1px solid var(--accent)",
      borderRadius: "2px",
    },
    ".cm-activeLineGutter": { backgroundColor: "transparent" },
    "&.cm-focused": { outline: "none" },
    ".cm-selectionBackground, ::selection": { backgroundColor: "var(--accent-dim) !important" },
    ".cm-tooltip": {
      backgroundColor: "#12151c",
      border: "1px solid var(--line)",
      color: "var(--fg)",
    },
    ".cm-tooltip-autocomplete ul li[aria-selected]": {
      backgroundColor: "var(--accent-dim)",
      color: "var(--fg)",
    },
  },
  { dark: true }
);
