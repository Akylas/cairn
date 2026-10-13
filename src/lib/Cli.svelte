<script>
  // The CLI reference, read from the binary itself. Nothing about the commands is written down
  // here: the list, the flags and the defaults all come from `cairn --help`, which clap
  // generates from the same definitions it parses with. A hand-written copy would be wrong
  // within a release.
  import { invoke, isTauri } from "./api.js";
  import { onMount } from "svelte";
  import { buildConfig } from "./buildconfig.svelte.js";

  let ref = $state(null);
  let error = $state("");
  let picked = $state("");
  let copied = $state("");

  onMount(async () => {
    try { ref = await invoke("cli_reference"); }
    catch (err) { error = String(err); }
  });

  async function copy(text) {
    try {
      await navigator.clipboard.writeText(text);
      copied = text;
      setTimeout(() => (copied = ""), 1200);
    } catch (err) { error = String(err); }
  }

  let current = $derived(ref?.commands?.find((c) => c.name === picked));
  // built by the app from the values it will run with, not worked out again here
  let script = $derived(buildConfig.lines.map((l) => l.line).join("\n"));
</script>

{#if error}<p class="warn">{error}</p>{/if}

<p class="lede">
  A command line runs exactly the step you name: it takes every path as a flag, and skips nothing
  unless you pass <code>--skip-existing</code>. The build record this app writes is never read
  back to decide what runs there.
</p>

<div class="where">
  {#if ref?.path}
    <p class="muted mono">{ref.path}</p>
  {:else if ref}
    <p class="warn">Not found — {ref.hint}</p>
    <div class="asline">
      <code>cargo build --release -p cairn-cli</code>
      <button class="ghost tiny" onclick={() => copy("cargo build --release -p cairn-cli")}>
        {copied.startsWith("cargo") ? "copied" : "copy"}
      </button>
    </div>
  {:else if isTauri}
    <p class="muted">Looking for the binary…</p>
  {:else}
    <p class="muted">Browser dev mode — the reference is read from the binary inside the app.</p>
  {/if}
</div>

{#if script}
  <h4>This build, as a command</h4>
  <div class="block">
    <p class="lede">
      What the Build tab is set to right now, as the app itself would run it: the options you have
      changed, plus any paths the app is configured with that the command line could not guess.
    </p>
    <pre class="script">{script}</pre>
    <button class="ghost" onclick={() => copy(script)}>
      {copied === script ? "copied" : "Copy all"}
    </button>
    <div class="lines">
      {#each buildConfig.lines as { line }}
        <div class="asline">
          <code>{line}</code>
          <button class="ghost tiny" onclick={() => copy(line)}>
            {copied === line ? "copied" : "copy"}
          </button>
        </div>
      {/each}
    </div>
  </div>
{/if}

{#if ref?.commands?.length}
  <h4>Commands <span class="muted">— click for its full help</span></h4>
  <div class="block">
    <div class="grid">
      {#each ref.commands as c}
        <button class="cmd" class:on={picked === c.name} onclick={() => (picked = picked === c.name ? "" : c.name)}>
          <span class="name">{c.name}</span>
          <span class="about">{c.about}</span>
        </button>
      {/each}
    </div>
    {#if current}
      <pre class="help">{current.help}</pre>
    {/if}
  </div>

  <details class="usage">
    <summary>cairn --help</summary>
    <pre class="help">{ref.usage}</pre>
  </details>
{/if}

<style>
  .lede { color: var(--text-2); font-size: 13px; margin: 0 0 10px; max-width: 70ch; }
  h4 { font-size: 11px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted-2);
       margin: 18px 0 8px; font-weight: 600; }
  h4 .muted { text-transform: none; letter-spacing: 0; font-weight: 400; }
  .where { margin-bottom: 4px; }
  .block { margin: 0; }
  .usage { margin-top: 16px; }
  .usage summary { cursor: pointer; font-size: 11px; text-transform: uppercase;
                   letter-spacing: .05em; color: var(--muted-2); }
  .mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
  .muted { color: var(--muted-2); }
  .warn { color: var(--warn); }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(210px, 1fr)); gap: 6px; }
  .cmd { display: flex; flex-direction: column; align-items: flex-start; gap: 2px; text-align: left;
         background: var(--surface); border: 1px solid var(--line-2); color: var(--text-2);
         padding: 8px 10px; border-radius: var(--r); }
  .cmd:hover { background: var(--hover); }
  .cmd.on { border-color: var(--accent); background: var(--accent-dim); color: var(--text); }
  .name { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px;
          color: var(--text); }
  .about { font-size: 11px; color: var(--muted-2); }
  .help { margin: 12px 0 0; padding: 10px; background: var(--bg); border: 1px solid var(--line-2);
          border-radius: var(--r); font-size: 11px; color: var(--text-3); max-height: 420px;
          overflow: auto; white-space: pre; }
  .script { margin: 0 0 8px; padding: 10px; background: var(--bg); border: 1px solid var(--line-2);
            border-radius: var(--r); font-size: 12px; color: var(--text); overflow-x: auto;
            white-space: pre; }
  .lines { margin-top: 10px; display: flex; flex-direction: column; gap: 6px; }
  .asline { display: flex; align-items: center; gap: 8px; background: var(--bg);
            border: 1px solid var(--line-2); border-radius: var(--r); padding: 7px 9px; }
  .asline code { flex: 1; color: var(--text-3); font-size: 11px; overflow-x: auto;
                 white-space: nowrap; }
  .tiny { padding: 2px 8px; font-size: 10px; }
</style>
