// What the Build view currently holds, shared so the CLI view can show the same run as a
// command line. One writer (Build), one reader (Cli) - it is not a general store.
export const buildConfig = $state({
  area: "",
  /** Step ids in the order they would run. */
  steps: [],
  /** step id -> { option key: value } */
  values: {},
  /** step id -> option definitions, for the flag names */
  defs: {},
  /** step id -> free-text tool arguments the form has no field for */
  extra: {},
  /** The `cairn` command line for each step, as the app itself builds it: [{ step, line }]. */
  lines: [],
});
