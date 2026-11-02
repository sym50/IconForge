"""Flatten vendored Lucide 0.468.0 SVG outlines into egui polylines.
Run from any directory. No runtime SVG renderer or font glyph dependency.
Source/license: ../assets/lucide (ISC).
"""
from pathlib import Path
import re, math, xml.etree.ElementTree as ET
root=Path(__file__).resolve().parents[1]
icons=['arrow-left','layout-grid','rotate-cw','history','cloud','sliders-horizontal','folder-open','file-text','app-window','ellipsis','check','undo-2','palette','search','chevron-right','shield-check','redo-2']
def arc(p,rx,ry,angle,large,sweep,end):
    rx,ry=abs(rx),abs(ry)
    if not rx or not ry or p==end:return [end]
    phi=math.radians(angle);cs,sn=math.cos(phi),math.sin(phi)
    dx,dy=(p[0]-end[0])/2,(p[1]-end[1])/2
    x,y=cs*dx+sn*dy,-sn*dx+cs*dy
    scale=max(1,math.sqrt(x*x/(rx*rx)+y*y/(ry*ry)));rx*=scale;ry*=scale
    factor=math.sqrt(max(0,(rx*rx*ry*ry-rx*rx*y*y-ry*ry*x*x)/(rx*rx*y*y+ry*ry*x*x)))
    if bool(large)==bool(sweep):factor=-factor
    cxp,cyp=factor*rx*y/ry,-factor*ry*x/rx
    cx,cy=cs*cxp-sn*cyp+(p[0]+end[0])/2,sn*cxp+cs*cyp+(p[1]+end[1])/2
    start=math.atan2((y-cyp)/ry,(x-cxp)/rx)
    stop=math.atan2((-y-cyp)/ry,(-x-cxp)/rx)
    delta=(stop-start)%(2*math.pi)
    if not sweep:delta-=2*math.pi
    count=max(2,math.ceil(abs(delta)*10))
    return [(cx+cs*rx*math.cos(start+delta*i/count)-sn*ry*math.sin(start+delta*i/count),cy+sn*rx*math.cos(start+delta*i/count)+cs*ry*math.sin(start+delta*i/count)) for i in range(1,count+1)]
def path(d):
    tokens=re.findall(r'[A-Za-z]|[-+]?(?:\d*\.\d+|\d+\.?\d*)(?:[eE][-+]?\d+)?',d)
    i=0;cmd=None;p=(0.,0.);start=p;line=[];lines=[];last_control=None
    def xy(x,y,relative):return (x+p[0],y+p[1]) if relative else (x,y)
    while i<len(tokens):
        if tokens[i].isalpha():cmd=tokens[i];i+=1
        upper=cmd.upper();rel=cmd.islower()
        if upper=='Z':
            line.append(start);p=start;lines.append(line);line=[];cmd=None;continue
        n={'M':2,'L':2,'H':1,'V':1,'C':6,'Q':4,'S':4,'T':2,'A':7}[upper]
        a=list(map(float,tokens[i:i+n]));assert len(a)==n;i+=n
        if upper=='M':
            if line:lines.append(line)
            p=xy(*a,rel);start=p;line=[p];cmd='l' if rel else 'L'
        elif upper=='L':p=xy(*a,rel);line.append(p)
        elif upper=='H':p=(p[0]+a[0] if rel else a[0],p[1]);line.append(p)
        elif upper=='V':p=(p[0],p[1]+a[0] if rel else a[0]);line.append(p)
        elif upper=='C':
            c1,c2,end=xy(*a[:2],rel),xy(*a[2:4],rel),xy(*a[4:],rel)
            for j in range(1,17):
                t=j/16;u=1-t
                line.append(tuple(u**3*p[k]+3*u*u*t*c1[k]+3*u*t*t*c2[k]+t**3*end[k] for k in range(2)))
            p=end;last_control=c2
        elif upper=='S':
            c1=(2*p[0]-last_control[0],2*p[1]-last_control[1]) if last_control else p
            c2,end=xy(*a[:2],rel),xy(*a[2:],rel)
            for j in range(1,17):
                t=j/16;u=1-t
                line.append(tuple(u**3*p[k]+3*u*u*t*c1[k]+3*u*t*t*c2[k]+t**3*end[k] for k in range(2)))
            p=end;last_control=c2
        elif upper=='Q':
            c1,end=xy(*a[:2],rel),xy(*a[2:],rel)
            for j in range(1,17):
                t=j/16;u=1-t;line.append(tuple(u*u*p[k]+2*u*t*c1[k]+t*t*end[k] for k in range(2)))
            p=end;last_control=c1
        elif upper=='T':
            c1=(2*p[0]-last_control[0],2*p[1]-last_control[1]) if last_control else p
            end=xy(*a,rel)
            for j in range(1,17):
                t=j/16;u=1-t;line.append(tuple(u*u*p[k]+2*u*t*c1[k]+t*t*end[k] for k in range(2)))
            p=end;last_control=c1
        elif upper=='A':
            end=xy(*a[5:],rel);line+=arc(p,*a[:5],end);p=end
        if upper not in ['C','S','Q','T']:last_control=None
    if line:lines.append(line)
    return lines
all_paths={}
for name in icons:
    lines=[]
    for node in ET.fromstring((root/'assets/lucide'/f'{name}.svg').read_text()).iter():
        tag=node.tag.split('}')[-1];a=node.attrib
        if tag=='svg':continue
        if tag=='path':lines+=path(a['d'])
        elif tag in ['polyline','polygon']:
            nums=list(map(float,re.findall(r'[-+\d.]+',a['points'])));pts=list(zip(nums[::2],nums[1::2]));lines.append(pts+[pts[0]] if tag=='polygon' else pts)
        elif tag=='line':lines.append([(float(a['x1']),float(a['y1'])),(float(a['x2']),float(a['y2']))])
        elif tag=='circle':
            x,y,r=[float(a[k]) for k in ['cx','cy','r']];lines.append([(x+r*math.cos(i*math.pi/16),y+r*math.sin(i*math.pi/16)) for i in range(33)])
        elif tag=='rect':
            x,y,w,h=[float(a.get(k,0)) for k in ['x','y','width','height']];r=float(a.get('rx',0))
            if not r:lines.append([(x,y),(x+w,y),(x+w,y+h),(x,y+h),(x,y)])
            else:
                pts=[]
                for cx,cy,angle in [(x+w-r,y+r,-90),(x+w-r,y+h-r,0),(x+r,y+h-r,90),(x+r,y+r,180)]:
                    pts += [(cx+r*math.cos(math.radians(angle+j*15)),cy+r*math.sin(math.radians(angle+j*15))) for j in range(7)]
                lines.append(pts+[pts[0]])
        else:raise ValueError(tag)
    assert lines and all(math.isfinite(v) and -2<=v<=26 for line in lines for p in line for v in p),name
    all_paths[name]=lines
variant=lambda s:''.join(word.title() for word in s.split('-'))
output=['//! Generated from Lucide 0.468.0 (ISC); license in assets/lucide/LICENSE.','//! Regenerate with tools/generate_icons.py.','#[derive(Clone, Copy, PartialEq)]','pub(crate) enum Icon { '+', '.join(map(variant,icons))+' }','pub(crate) fn draw_icon(painter: &egui::Painter, rect: egui::Rect, icon: Icon, color: egui::Color32) {','    let paths: &[&[(f32,f32)]] = match icon {']
for name,lines in all_paths.items():
    output.append('        Icon::'+variant(name)+' => &[')
    for line in lines:output.append('            &['+','.join(f'({x:.4f},{y:.4f})' for x,y in line)+'],')
    output.append('        ],')
output+=['    };','    let scale=rect.width().min(rect.height())/24.0;','    let origin=rect.center()-egui::vec2(12.0*scale,12.0*scale);','    let stroke=egui::Stroke::new(2.0*scale,color);','    for path in paths {','        let points: Vec<_> = path.iter().map(|&(x,y)|origin+egui::vec2(x*scale,y*scale)).collect();','        if let (Some(first),Some(last))=(points.first(),points.last()) {','            painter.circle_filled(*first,stroke.width/2.0,color);','            painter.circle_filled(*last,stroke.width/2.0,color);','        }','        painter.add(egui::Shape::line(points,stroke));','    }','}']
(root/'src/ui_icons.rs').write_text('\n'.join(output)+'\n',encoding='utf-8')
(root/'assets/lucide/SOURCE.txt').write_text('Lucide 0.468.0\nhttps://unpkg.com/lucide-static@0.468.0/\nhttps://lucide.dev/\nVendored SVGs are build-time sources; generated egui outlines require no runtime dependency.\n',encoding='utf-8')
print('Generated',len(icons),'icons')
