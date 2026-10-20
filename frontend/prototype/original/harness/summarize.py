import json,sys,re
def fam(f):
    if not f: return ''
    if f.startswith('"Inter Variable"'): return 'NUM'
    if 'SF Pro Text' in f: return 'SANS'
    if 'PingFang' in f: return 'PF'
    return f[:20]
def rgb(c):
    if not c: return ''
    m=re.match(r'rgba?\((\d+), (\d+), (\d+)(?:, ([\d.]+))?\)',c)
    if m:
        r,g,b,a=m.groups(); h='#%02x%02x%02x'%(int(r),int(g),int(b))
        return h if a is None else f'{h}/{float(a):.2f}'
    return c[:60]
def brd(b):
    if not b or b.startswith('0px'): return ''
    m=re.match(r'([\d.]+px) (\w+) (.*)',b); 
    return f'{m.group(1)} {rgb(m.group(3))}' if m else b
d=json.load(open(sys.argv[1]))
for s,v in d['selectors'].items():
    if v is None: print('--',s,'(none)'); continue
    parts=[f"{v.get('font-size')}/{v.get('line-height')} w{v.get('font-weight')} {fam(v.get('font-family'))}", f"ls{v.get('letter-spacing','')}" if v.get('letter-spacing','normal')!='normal' else '', 'c'+rgb(v.get('color'))]
    bg=v.get('background-color'); 
    if bg and bg!='rgba(0, 0, 0, 0)': parts.append('bg'+rgb(bg))
    if v.get('background-image'): parts.append('bgimg:'+v['background-image'][:70])
    for k in ['border-top','border-bottom','border-left']:
        b=brd(v.get(k)); 
        if b: parts.append(k.split('-')[1][0]+':'+b)
    if v.get('border-radius','0px')!='0px': parts.append('r'+v['border-radius'])
    if v.get('padding','0px')!='0px': parts.append('p'+v['padding'])
    if v.get('margin','0px')!='0px': parts.append('m'+v['margin'])
    if v.get('gap') and v.get('gap')!='normal': parts.append('gap'+v['gap'])
    if v.get('box-shadow'): parts.append('sh:'+v['box-shadow'][:80])
    if v.get('grid-template-columns'): parts.append('cols:'+v['grid-template-columns'][:60])
    if v.get('grid-template-rows'): parts.append('rows:'+v['grid-template-rows'][:40])
    if v.get('font-variant-numeric','normal')!='normal': parts.append('fvn:'+v['font-variant-numeric'])
    if v.get('text-transform'): parts.append('tt:'+v['text-transform'])
    if v.get('min-height') not in (None,'auto','0px'): parts.append('minh'+v['min-height'])
    parts.append('box'+v['_box']); parts.append('«'+v['_text'][:24].replace('\n',' ')+'»')
    print('##',s,' | '.join(p for p in parts if p))
