import fs from "node:fs";
import path from "node:path";
import ts from "typescript";

const root = process.cwd();
const en = JSON.parse(fs.readFileSync(path.join(root, "src/i18n/locales/en/translation.json"), "utf8"));
const files = [];
function visit(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) visit(full);
    else if (/\.(ts|tsx)$/.test(entry.name)) files.push(full);
  }
}
visit(path.join(root, "src"));
function lookup(key) {
  return key.split(".").reduce((value, part) => value && value[part], en);
}
const report = [];
const outRoot = path.join(process.env.TEMP || "C:/Temp", "masr-en-transform");
fs.rmSync(outRoot, { recursive: true, force: true });
fs.mkdirSync(outRoot, { recursive: true });

function expressionFor(value, values) {
  if (typeof value !== "string") return null;
  const replaced = value.replace(/{{\s*([\w.]+)\s*}}/g, (_, name) => `\${(${values.get(name) || name})}`);
  if (!replaced.includes("${")) return JSON.stringify(value);
  return `\`${replaced.replace(/`/g, "\\`")}\``;
}

function optionValues(node, sf) {
  const values = new Map();
  let fallback = null;
  if (!node) return { values, fallback };
  if (ts.isStringLiteralLike(node)) return { values, fallback: node.getText(sf) };
  if (!ts.isObjectLiteralExpression(node)) return { values, fallback };
  for (const property of node.properties) {
    if (ts.isPropertyAssignment(property)) {
      const name = property.name.getText(sf).replace(/["']/g, "");
      if (name === "defaultValue" && ts.isStringLiteralLike(property.initializer)) fallback = property.initializer.getText(sf);
      else values.set(name, property.initializer.getText(sf));
    } else if (ts.isShorthandPropertyAssignment(property)) {
      values.set(property.name.text, property.name.text);
    }
  }
  return { values, fallback };
}

function replacementFor(node, sf) {
  const [keyNode, options] = node.arguments;
  if (!keyNode || !ts.isStringLiteralLike(keyNode)) return null;
  const key = keyNode.text;
  const { values, fallback } = optionValues(options, sf);
  const direct = lookup(key);
  if (typeof direct === "string") return expressionFor(direct, values);
  const one = lookup(`${key}_one`);
  const other = lookup(`${key}_other`);
  if (typeof one === "string" && typeof other === "string" && values.has("count")) {
    const count = values.get("count");
    const oneExpr = expressionFor(one, values);
    const otherExpr = expressionFor(other, values);
    return `(${count} === 1 ? ${oneExpr} : ${otherExpr})`;
  }
  return fallback;
}

const skipped = [];
let transformed = 0;
for (const file of files) {
  const source = fs.readFileSync(file, "utf8");
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const edits = [];
  function walk(node) {
    const isT = ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === "t";
    const isI18nT = ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression) && node.expression.expression.getText(sf) === "i18n" && node.expression.name.text === "t";
    if (isT || isI18nT) {
      const [key] = node.arguments;
      const keyText = key && ts.isStringLiteralLike(key) ? key.text : null;
      report.push({ file: path.relative(root, file), line: sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1, key: keyText, resolved: keyText ? lookup(keyText) : null, args: node.arguments.map((arg) => arg.getText(sf)), text: node.getText(sf) });
      const replacement = replacementFor(node, sf);
      if (replacement) edits.push({ start: node.getStart(sf), end: node.getEnd(), replacement });
      else skipped.push({ file: path.relative(root, file), line: sf.getLineAndCharacterOfPosition(node.getStart(sf)).line + 1, text: node.getText(sf) });
    }
    ts.forEachChild(node, walk);
  }
  walk(sf);
  const nonOverlapping = edits.sort((a, b) => b.start - a.start).filter((edit, index, all) => !all.slice(0, index).some((higher) => higher.start < edit.end));
  let updated = source;
  for (const edit of nonOverlapping) {
    updated = updated.slice(0, edit.start) + edit.replacement + updated.slice(edit.end);
    transformed += 1;
  }
  updated = updated
    .replace(/^import \{ useTranslation \} from "react-i18next";\r?\n/gm, "")
    .replace(/^import \{ Trans, useTranslation \} from "react-i18next";\r?\n/gm, 'import { Trans } from "react-i18next";\n')
    .replace(/^\s*const \{ t \} = useTranslation\(\);\r?\n/gm, "")
    .replace(/const \{ t, i18n \} = useTranslation\(\);/g, "const { i18n } = useTranslation();");
  const target = path.join(outRoot, path.relative(root, file));
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, updated);
}
const unresolved = report.filter((entry) => entry.resolved === undefined || entry.resolved === null || entry.key === null);
const nonString = report.filter((entry) => typeof entry.resolved !== "string");
console.log(JSON.stringify({
  calls: report.length,
  transformed,
  files: new Set(report.map((entry) => entry.file)).size,
  unresolved,
  nonString,
  skipped,
}, null, 2));
