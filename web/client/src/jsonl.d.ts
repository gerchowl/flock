/** JSONL imports are validated data, loaded by the fixture Vite plugin. */
declare module "*.jsonl" {
  const text: string;
  export default text;
}
