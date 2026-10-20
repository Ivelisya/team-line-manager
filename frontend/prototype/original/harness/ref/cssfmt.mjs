import fs from 'fs';
const src = fs.readFileSync(process.argv[2],'utf8');
let out='', depth=0, i=0, inStr=null;
for (const ch of src) {
  if (inStr) { out+=ch; if (ch===inStr) inStr=null; continue; }
  if (ch==='"'||ch==="'") { inStr=ch; out+=ch; continue; }
  if (ch==='{') { depth++; out+=' {\n'+'  '.repeat(depth); continue; }
  if (ch==='}') { depth=Math.max(0,depth-1); out=out.replace(/\s*$/,''); out+='\n'+'  '.repeat(depth)+'}\n'+'  '.repeat(depth); continue; }
  if (ch===';') { out+=';\n'+'  '.repeat(depth); continue; }
  out+=ch;
}
fs.writeFileSync(process.argv[3], out.replace(/\n\s*\n/g,'\n'));
