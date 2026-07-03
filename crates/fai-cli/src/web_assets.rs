#[cfg(test)]
pub(crate) fn generate_html_loader(wasm_filename: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>FAI</title>
<style>*{{margin:0;padding:0;box-sizing:border-box}}body{{font-family:-apple-system,system-ui,sans-serif;background:#fafafa;color:#1a1a1a;font-size:16px;display:flex;justify-content:center;padding-top:48px}}#app{{min-width:200px}}</style>
</head>
<body>
<div id="app"></div>
<pre id="output" style="display:none"></pre>
<script>
const app=document.getElementById('app'),output=document.getElementById('output');
let instance,state='{{}}';
const FAI_DEBUG=window.__FAI_DEBUG__===true||new URLSearchParams(location.search).get('fai_debug')==='1'||localStorage.getItem('fai_debug')==='1';
function debugLog(...args){{if(FAI_DEBUG)console.log(...args)}}
const QNAN=0x7FFC000000000000n,SIGN=0x8000000000000000n,OBJ_MASK=QNAN|SIGN;
const TAG_INT=0x0004000000000000n,TAG_BOOL=0x0003000000000000n,TAG_NULL=0x0001000000000000n;
const NULL_VAL=QNAN|TAG_NULL,INT_MASK=QNAN|TAG_INT,BOOL_MASK=QNAN|TAG_BOOL;
function jsToWasm(v){{
  if(v===null||v===undefined)return QNAN|TAG_NULL;
  if(typeof v==='boolean')return QNAN|TAG_BOOL|BigInt(v?1:0);
  if(typeof v==='number'){{if(Number.isInteger(v))return QNAN|TAG_INT|BigInt.asUintN(32,BigInt(v));const buf=new ArrayBuffer(8);new Float64Array(buf)[0]=v;return new BigInt64Array(buf)[0]}}
  if(typeof v==='string')return writeStrToWasm(v);
  if(Array.isArray(v)){{const dv=new DataView(instance.exports.memory.buffer);const base=instance.exports.__heap_ptr.value;const logsz=8+v.length*8;const addr=base+8;const end=(base+8+logsz+7)&~7;instance.exports.__heap_ptr.value=end;dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);const items=v.map(i=>jsToWasm(i));const m2=instance.exports.memory.buffer;for(let i=0;i<items.length;i++){{const bi=new BigInt64Array(m2,addr+8+i*8,1);bi[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{const keys=Object.keys(v);const base=instance.exports.__heap_ptr.value;const cap=Math.max(keys.length,16);const logsz=8+cap*16;const addr=base+8;const dv=new DataView(instance.exports.memory.buffer);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);instance.exports.__heap_ptr.value=(base+8+logsz+7)&~7;for(let i=0;i<keys.length;i++){{const kv=writeStrToWasm(keys[i]);const vv=jsToWasm(v[keys[i]]);const ea=addr+8+i*16;const m2=instance.exports.memory.buffer;const bi=new BigInt64Array(m2,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
  return QNAN|TAG_NULL;
}}
function wasmToJs(v){{
  // NaN-box tag discrimination. TAG_INT (0x0004) overlaps with QNAN's
  // bit 50 so `(n & INT_MASK) === INT_MASK` matches EVERY NaN-boxed
  // value. Order the checks so more-specific patterns (object, bool,
  // null) win before the Int fallback. Mirrors fai-core/src/value.rs
  // `is_int` / `is_object` semantics.
  const n=BigInt(v);
  if(n===NULL_VAL)return null;
  if((n&OBJ_MASK)===OBJ_MASK){{const a=Number(n&0x0000FFFFFFFFFFFFn);const dv=new DataView(instance.exports.memory.buffer);const tag=dv.getInt32(a,true);
    if(tag===0){{const l=dv.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}
    if(tag===1||tag===2){{const cnt=dv.getInt32(a+4,true);const r=[];for(let i=0;i<cnt;i++){{const bi=new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1);r.push(wasmToJs(bi[0]))}}return r}}
    if(tag===3){{const cnt=dv.getInt32(a+4,true);const r={{}};for(let i=0;i<cnt;i++){{const ea=a+8+i*16;const bi=new BigInt64Array(instance.exports.memory.buffer,ea,2);const k=wasmToJs(bi[0]);const val=wasmToJs(bi[1]);if(typeof k==='string')r[k]=val}}return r}}
    // Fall through for other tags (Tuple, Closure, NativeFn, Instance, Module).
    return null;
  }}
  if((n&BOOL_MASK)===BOOL_MASK)return(n&1n)===1n;
  // Int: high 16 bits == QNAN (0x7FFC), sign bit clear, not null/bool.
  if((n&QNAN)===QNAN)return Number(BigInt.asIntN(32,n&0xFFFFFFFFn));
  // Raw f64 (non-NaN) — reinterpret bits as double.
  const buf=new ArrayBuffer(8);new BigInt64Array(buf)[0]=n;return new Float64Array(buf)[0];
}}
function readStr(p,l){{return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,p,l))}}
function faiJsonQueryEval(root,path){{var steps=[];var i=0;var n=path.length;function quoted(at){{var j=at+1;var out='';while(j<n){{var q=path[j];if(q==='"')return[out,j+1];if(q==='\\'&&j+1<n){{out+=path[j+1];j+=2}}else{{out+=q;j+=1}}}}return null}}while(i<n){{var c=path[i];if(c===' '||c==='\t'||c==='\n'||c==='\r'||c==='|'||c==='?'){{i+=1;continue}}if(c==='.'){{if(i+1<n&&path[i+1]==='.'){{steps.push({{d:1}});i+=2}}else{{i+=1}}continue}}if(c==='"'){{var q1=quoted(i);if(!q1)return null;steps.push({{f:q1[0]}});i=q1[1];continue}}if(c==='['){{i+=1;while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i<n&&path[i]===']'){{steps.push({{a:1}});i+=1;continue}}if(i<n&&path[i]==='"'){{var q2=quoted(i);if(!q2)return null;i=q2[1];while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i>=n||path[i]!==']')return null;steps.push({{f:q2[0]}});i+=1;continue}}var st=i;if(i<n&&path[i]==='-')i+=1;while(i<n&&path[i]>='0'&&path[i]<='9')i+=1;var ds=path.slice(st,i);if(ds===''||ds==='-')return null;while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i>=n||path[i]!==']')return null;steps.push({{n:parseInt(ds,10)}});i+=1;continue}}if(c===']')return null;var s0=i;while(i<n&&'.|[]?"'.indexOf(path[i])<0&&' \t\n\r'.indexOf(path[i])<0)i+=1;if(i===s0)return null;steps.push({{f:path.slice(s0,i)}})}}var cur=[root];for(var k=0;k<steps.length;k++){{var stp=steps[k];var next=[];for(var m=0;m<cur.length;m++){{var v=cur[m];if(stp.f!==undefined){{if(v&&typeof v==='object'&&!Array.isArray(v)&&Object.prototype.hasOwnProperty.call(v,stp.f))next.push(v[stp.f])}}else if(stp.a){{if(Array.isArray(v)){{for(var x=0;x<v.length;x++)next.push(v[x])}}else if(v&&typeof v==='object'){{var ks=Object.keys(v);for(var x2=0;x2<ks.length;x2++)next.push(v[ks[x2]])}}}}else if(stp.n!==undefined){{if(Array.isArray(v)){{var ix=stp.n<0?v.length+stp.n:stp.n;if(ix>=0&&ix<v.length)next.push(v[ix])}}}}else if(stp.d){{var stack=[v];while(stack.length){{var t=stack.pop();next.push(t);if(Array.isArray(t)){{for(var y=t.length-1;y>=0;y--)stack.push(t[y])}}else if(t&&typeof t==='object'){{var tk=Object.keys(t);for(var y2=tk.length-1;y2>=0;y2--)stack.push(t[tk[y2]])}}}}}}}}cur=next}}return cur}}

function writeStr(p,s){{const b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p).set(b);return b.length}}
function writeStrToWasm(s){{const b=new TextEncoder().encode(s);const base=instance.exports.__heap_ptr.value;const logsz=8+b.length;const h=base+8;const m=new Uint8Array(instance.exports.memory.buffer);const d=new DataView(instance.exports.memory.buffer);d.setInt32(base,1,true);d.setInt32(base+4,logsz,true);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{const n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{const a=Number(n&0x0000FFFFFFFFFFFFn);const d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{const l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function invokeExport(name,...args){{const fn=instance.exports[name];if(!fn){{console.warn('FAI invokeExport missing export', name);return;}}debugLog('FAI invokeExport:start', {{name,args}});try{{const result=fn(...args);debugLog('FAI invokeExport:end', {{name,result}});return result;}}catch(e){{console.error('FAI invokeExport:failed', {{name,args,error:e}});throw e;}}}}
let asyncRootDone=false;
function rootResultText(result){{const v=wasmToJs(result);if(Array.isArray(v))return JSON.stringify(v);if(v===null||v===undefined)return'';return String(v);}}
function publishRootResult(result){{window.__FAI_ROOT_RESULT_TEXT=rootResultText(result);window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;const s=readNanBoxedStr(result);if(s&&s.startsWith('{{'))state=s;}}
function pumpAsync(){{if(!instance||!instance.exports.__fai_poll||asyncRootDone)return 0;const status=invokeExport('__fai_poll');if(status===2){{asyncRootDone=true;if(instance.exports.__fai_task_result)publishRootResult(invokeExport('__fai_task_result',1));}}else if(status===3){{asyncRootDone=true;window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;console.error('FAI async task failed',instance.exports.__fai_task_result?invokeExport('__fai_task_result',1):null)}}return status;}}
function startFai(){{window.__FAI_ROOT_DONE=false;window.__FAI_ROOT_RESULT_TEXT='';window.__FAI_ROOT_STARTED_AT=performance.now();window.__FAI_ROOT_FINISHED_AT=undefined;if(instance.exports._start_async){{asyncRootDone=false;invokeExport('_start_async');pumpAsync();}}else{{publishRootResult(invokeExport('_start'));}}}}
var __faiHostOpResults={{}};
function readHostOpArgs(count,ptr){{var out=[];for(var i=0;i<count;i++)out.push(new BigInt64Array(instance.exports.memory.buffer,ptr+i*8,1)[0]);return out}}
function fetchHeaders(headers){{var out={{}};headers.forEach(function(v,k){{out[k]=v}});return out}}
function hostOpBegin(taskId,opKind,count,argsPtr,scheduler){{var args=readHostOpArgs(count,argsPtr),method={{1:'GET',2:'POST',3:'PUT',4:'PATCH',5:'DELETE'}}[opKind];function done(val){{__faiHostOpResults[taskId]=val;if(instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);scheduler()}}if(opKind===6||opKind===7){{done(NULL_VAL);return}}if(opKind===8||opKind===10){{done(jsToWasm(false));return}}if(opKind===9){{done(jsToWasm([]));return}}if(opKind===12){{done(jsToWasm(-1));return}}if(opKind>=11&&opKind<=15){{done(NULL_VAL);return}}if(!method){{done(NULL_VAL);return}}var hasBody=opKind===2||opKind===3||opKind===4,url=String(wasmToJs(args[0])||''),body=hasBody?String(wasmToJs(args[1])||''):undefined,headersArg=args[hasBody?2:1],headers=headersArg===undefined?{{}}:(wasmToJs(headersArg)||{{}}),opts={{method:method,headers:headers}};if(hasBody)opts.body=body;fetch(url,opts).then(function(r){{return r.text().then(function(t){{done(jsToWasm({{status:r.status,body:t||'',headers:fetchHeaders(r.headers)}}))}})}}).catch(function(e){{console.error('FAI host http op failed',e);done(NULL_VAL)}})}}
function hostOpResult(taskId){{var v=__faiHostOpResults[taskId];delete __faiHostOpResults[taskId];return v===undefined?NULL_VAL:v}}
function callWasm(name,arg){{const fn=instance.exports[name];if(!fn){{console.warn('FAI callWasm missing export', name);return;}}console.log('FAI callWasm', {{name,arg:arg||''}});const ptr=writeStrToWasm(arg||'');const result=invokeExport(name,ptr);return readNanBoxedStr(result)}}
function rerender(stateArg){{debugLog('FAI rerender', {{stateArg:stateArg||''}});if(instance.exports.render){{callWasm('render',stateArg||'')}}else if(instance&&instance.exports&&(instance.exports._start_async||instance.exports._start)){{startFai();}}else{{console.warn('FAI rerender missing render and _start')}}}}
function wireEvents(){{debugLog('FAI wireEvents');document.querySelectorAll('[data-fai-click]').forEach(el=>{{const h=el.getAttribute('data-fai-click');el.onclick=()=>{{console.log('FAI click', h);callWasm(h);rerender('')}}}});document.querySelectorAll('[data-fai-input]').forEach(el=>{{const h=el.getAttribute('data-fai-input');el.oninput=()=>{{const d=JSON.stringify({{_state:JSON.parse(state||'{{}}'),_value:el.value}});console.log('FAI input', {{handler:h,value:el.value}});state=callWasm(h,d);rerender(state)}}}})}}
function handleEvent(id){{const fn=instance.exports.invokeHandler;if(!fn){{console.warn('FAI handleEvent: invokeHandler not exported');return;}}const boxed=BigInt(id)|0x7FFC000400000000n;debugLog('FAI handleEvent',{{id}});try{{fn(boxed)}}catch(e){{console.error('FAI handleEvent failed',{{id,error:e}})}}}}
function handleInputEvent(id,value){{const fn=instance.exports.invokeChangeHandler;if(!fn){{console.warn('FAI handleInputEvent: invokeChangeHandler not exported');return;}}const boxedId=BigInt(id)|0x7FFC000400000000n;const boxedStr=writeStrToWasm(value);debugLog('FAI handleInputEvent',{{id,value}});try{{fn(boxedId,boxedStr)}}catch(e){{console.error('FAI handleInputEvent failed',{{id,error:e}})}}}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}while(op.childNodes.length>nc.length)op.removeChild(op.lastChild)}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
const env={{
  print(p,l){{const text=readStr(p,l);debugLog('FAI print', text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file(){{return -1}},write_file(){{return -1}},now_ms(){{return Date.now()}},random(){{return Math.random()}},sleep_ms(){{throw new Error('FAI legacy sleep_ms is disabled; sleep() must lower through the async scheduler')}},host_set_timer(taskId,ms){{setTimeout(function(){{if(instance&&instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);pumpAsync()}},Math.max(0,ms|0))}},host_op_begin(taskId,opKind,count,argsPtr){{hostOpBegin(taskId,opKind,count,argsPtr,pumpAsync)}},host_op_result(taskId){{return hostOpResult(taskId)}},
  call_ffi(){{return 0x7FFC000100000000n}},run_all(){{throw new Error('FAI legacy run_all is disabled; all() must lower through the async scheduler')}},
  spawn(closureVal){{var cv=closureVal;setTimeout(function(){{var n=BigInt(cv);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return;var tag=dv.getInt32(a,true);if(tag!==4)return;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}if(typeof faiServiceScheduler==='function')faiServiceScheduler()}},0);return 0x7FFC000200000000n}},
  http_post(a,b,c,d,e){{try{{const x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html(p,l){{const html=readStr(p,l);console.log('FAI set_html', {{length:l}});debugLog('FAI set_html:preview', html.slice(0,240));morphDom(app,html,false);wireEvents()}},
  set_html_at(a,b,p,l){{const selector=readStr(a,b);const html=readStr(p,l);let root=document.querySelector(selector);if(!root&&selector.startsWith('#')){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root);}}if(!root){{console.error('FAI set_html_at missing root', selector);return;}}console.log('FAI set_html_at', {{selector,length:l}});debugLog('FAI set_html_at:preview', {{selector,html:html.slice(0,240)}});morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse(p,l){{try{{const s=readStr(p,l);const v=JSON.parse(s);return jsToWasm(v)}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify(v){{try{{const j=wasmToJs(v);return writeStrToWasm(JSON.stringify(j))}}catch(e){{return writeStrToWasm('null')}}}},
  json_query(p,l,qp,ql){{try{{const root=JSON.parse(readStr(p,l));const m=faiJsonQueryEval(root,readStr(qp,ql));if(m===null)return QNAN|TAG_NULL;return jsToWasm(m)}}catch(e){{return QNAN|TAG_NULL}}}},
  json_query_page(p,l,qp,ql,off,lim){{try{{const root=JSON.parse(readStr(p,l));const m=faiJsonQueryEval(root,readStr(qp,ql));if(m===null)return QNAN|TAG_NULL;const total=m.length;let s=Math.max(0,off|0);if(s>total)s=total;const t=Math.max(0,lim|0);return jsToWasm({{total:total,items:m.slice(s,s+t)}})}}catch(e){{return QNAN|TAG_NULL}}}},
  json_format(p,l){{try{{return writeStrToWasm(JSON.stringify(JSON.parse(readStr(p,l)),null,2))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_minify(p,l){{try{{return writeStrToWasm(JSON.stringify(JSON.parse(readStr(p,l))))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_valid(p,l){{try{{JSON.parse(readStr(p,l));return 1}}catch(e){{return 0}}}},
  json_stringify_pretty(v){{try{{return writeStrToWasm(JSON.stringify(wasmToJs(v),null,2))}}catch(e){{return writeStrToWasm('null')}}}},
  remote_call(a,b,c,d,e,f,g,h){{const u=readStr(a,b),fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);const body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});console.log('FAI remote_call request', {{url:u,fn:fn_name,args:ar,hash:ha}});try{{const x=new XMLHttpRequest();x.open('POST',u.replace(/\/+$/,'')+'/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body);const resp=JSON.parse(x.responseText);console.log('FAI remote_call response', {{fn:fn_name,ok:resp.ok,value:resp.value,error:resp.error}});if(resp.ok)return jsToWasm(resp.value);console.warn('FAI remote_call returned error', resp);return NULL_VAL}}catch(e){{console.error('FAI remote_call failed', e);return NULL_VAL}}}},
  float_to_str(v,p){{const s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);const b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  replace_location(p,l){{window.location.replace(readStr(p,l))}},
  storage_get(kp,kl,bp){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return -1;const b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_get_str(kp,kl){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return NULL_VAL;return writeStrToWasm(v)}}catch(e){{return NULL_VAL}}}},
  file_read_str(){{return NULL_VAL}},
  storage_set(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear(){{try{{window.localStorage.clear()}}catch(e){{}}}},
  env_get(){{return NULL_VAL}},
  env_load(){{return 0}},
  secrets_get(){{return NULL_VAL}},
  secrets_has(){{return 0}},
  secrets_available(){{return 0}},
  secrets_reveal(){{return NULL_VAL}},
  secrets_bearer(){{return NULL_VAL}},
  secrets_basic(){{return NULL_VAL}},
  secrets_header(){{return NULL_VAL}},
  secrets_refresh(){{return 0}},
  event_on(){{return NULL_VAL}},
  event_once(){{return NULL_VAL}},
  event_off(){{return 0}},
  event_emit(){{}},
  event_subscribers(){{return 0}},
  event_clear(){{}},
  event_clear_all(){{}},
  event_emit_deferred(){{}},
  event_drain(){{}},
  event_queue_len(){{return 0}},
  __fai_set_trap_msg(p,l){{console.error('FAI trap:',readStr(p,l))}},
  __fai_trap_report(code,a,b){{console.error('FAI trap report',{{code,a,b}})}},
  __fai_alloc_event(){{}},
  __fai_free_event(){{}},
  __fai_ownership_event(){{}},
  __fai_debug_function_call(){{}}
}};
fetch('/{}').then(r=>r.arrayBuffer()).then(b=>WebAssembly.instantiate(b,{{env}})).then(r=>{{
  instance=r.instance;window.__fai_live_objects=function(){{return instance&&instance.exports.__live_objects?instance.exports.__live_objects.value:null}};debugLog('FAI wasm instantiated', Object.keys(instance.exports));startFai();
}}).catch(e=>{{app.innerHTML='<p style="color:red;padding:20px">Error: '+e.message+'</p>'}});
</script>
</body>
</html>"#,
        wasm_filename
    )
}

pub(crate) fn generate_html_page() -> String {
    include_str!("../templates/html_page.html").to_string()
}

/// Default forui stylesheet. Shipped alongside the runtime JS by
/// `forai build --html`. Emits iOS-leaning defaults for every
/// component kind the html-forui renderer supports.
///
/// Components opt in via the `fai-<kind>` class the renderer emits
/// (e.g. `fai-vstack`, `fai-button`, `fai-segmented`). User-facing
/// modifier styles (padding/background/foreground/...) remain inline
/// and override these defaults by CSS specificity (inline > class).
pub(crate) fn generate_forui_css() -> String {
    include_str!("../templates/forui.css").to_string()
}

/// Pull the raw `fai-dbg` custom-section payload (JSON) out of a wasm
/// binary, if present. See `fai-codegen-wasm/src/debug_info.rs` for the
/// shape. Returns `None` for pre-plan-116 binaries.
pub(crate) fn extract_dbg_section(wasm: &[u8]) -> Option<Vec<u8>> {
    use wasmparser::{Parser, Payload};
    for payload in Parser::new(0).parse_all(wasm) {
        if let Ok(Payload::CustomSection(reader)) = payload {
            if reader.name() == fai_codegen_wasm::debug_info::DBG_SECTION_NAME {
                return Some(reader.data().to_vec());
            }
        }
    }
    None
}

pub(crate) fn generate_runtime_js(wasm_filename: &str) -> String {
    format!(
        r#"const app=document.getElementById('app'),output=document.getElementById('output');
let instance,state='{{}}'
const FAI_DEBUG=window.__FAI_DEBUG__===true||new URLSearchParams(location.search).get('fai_debug')==='1'||localStorage.getItem('fai_debug')==='1';
function debugLog(){{if(FAI_DEBUG)console.log.apply(console,arguments)}}
const QNAN=0x7FFC000000000000n,SIGN=0x8000000000000000n,OBJ_MASK=QNAN|SIGN;
const TAG_INT=0x0004000000000000n,TAG_BOOL=0x0003000000000000n,TAG_NULL=0x0001000000000000n;
const NULL_VAL=QNAN|TAG_NULL,INT_MASK=QNAN|TAG_INT,BOOL_MASK=QNAN|TAG_BOOL;
function jsToWasm(v){{
  if(v===null||v===undefined)return QNAN|TAG_NULL;
  if(typeof v==='boolean')return QNAN|TAG_BOOL|BigInt(v?1:0);
  if(typeof v==='number'){{if(Number.isInteger(v))return QNAN|TAG_INT|BigInt.asUintN(32,BigInt(v));var buf=new ArrayBuffer(8);new Float64Array(buf)[0]=v;return new BigInt64Array(buf)[0]}}
  if(typeof v==='string')return writeStrToWasm(v);
  if(Array.isArray(v)){{var base=instance.exports.__heap_ptr.value,logsz=8+v.length*8,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);faiLeakAlloc(addr,logsz,true);faiOwnershipAlloc(addr);var items=v.map(function(i){{return jsToWasm(i)}});m=instance.exports.memory.buffer;for(var i=0;i<items.length;i++){{new BigInt64Array(m,addr+8+i*8,1)[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{var keys=Object.keys(v),base=instance.exports.__heap_ptr.value,cap=Math.max(keys.length,16),logsz=8+cap*16,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);faiLeakAlloc(addr,logsz,true);faiOwnershipAlloc(addr);for(var i=0;i<keys.length;i++){{var kv=writeStrToWasm(keys[i]),vv=jsToWasm(v[keys[i]]),ea=addr+8+i*16;m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
  return QNAN|TAG_NULL;
}}
function wasmToJs(v){{
  // See matching comment in generate_runtime_js above — INT_MASK
  // aliases QNAN due to a tag-bit overlap, so object/bool/null
  // checks must come before the Int fallback.
  var n=BigInt(v);if(n===NULL_VAL)return null;
  if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn);var dv=new DataView(instance.exports.memory.buffer);var tag=dv.getInt32(a,true);
    if(tag===0){{var l=dv.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}
    if(tag===1||tag===2){{var cnt=dv.getInt32(a+4,true),r=[];for(var i=0;i<cnt;i++){{r.push(wasmToJs(new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1)[0]))}}return r}}
    if(tag===3){{var cnt=dv.getInt32(a+4,true),r={{}};for(var i=0;i<cnt;i++){{var ea=a+8+i*16,bi=new BigInt64Array(instance.exports.memory.buffer,ea,2),k=wasmToJs(bi[0]),val=wasmToJs(bi[1]);if(typeof k==='string')r[k]=val}}return r}}
    return null;
  }}
  if((n&BOOL_MASK)===BOOL_MASK)return(n&1n)===1n;
  if((n&QNAN)===QNAN)return Number(BigInt.asIntN(32,n&0xFFFFFFFFn));
  var buf=new ArrayBuffer(8);new BigInt64Array(buf)[0]=n;return new Float64Array(buf)[0];
}}
function readStr(p,l){{return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,p,l))}}
function faiJsonQueryEval(root,path){{var steps=[];var i=0;var n=path.length;function quoted(at){{var j=at+1;var out='';while(j<n){{var q=path[j];if(q==='"')return[out,j+1];if(q==='\\'&&j+1<n){{out+=path[j+1];j+=2}}else{{out+=q;j+=1}}}}return null}}while(i<n){{var c=path[i];if(c===' '||c==='\t'||c==='\n'||c==='\r'||c==='|'||c==='?'){{i+=1;continue}}if(c==='.'){{if(i+1<n&&path[i+1]==='.'){{steps.push({{d:1}});i+=2}}else{{i+=1}}continue}}if(c==='"'){{var q1=quoted(i);if(!q1)return null;steps.push({{f:q1[0]}});i=q1[1];continue}}if(c==='['){{i+=1;while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i<n&&path[i]===']'){{steps.push({{a:1}});i+=1;continue}}if(i<n&&path[i]==='"'){{var q2=quoted(i);if(!q2)return null;i=q2[1];while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i>=n||path[i]!==']')return null;steps.push({{f:q2[0]}});i+=1;continue}}var st=i;if(i<n&&path[i]==='-')i+=1;while(i<n&&path[i]>='0'&&path[i]<='9')i+=1;var ds=path.slice(st,i);if(ds===''||ds==='-')return null;while(i<n&&' \t\n\r'.indexOf(path[i])>=0)i+=1;if(i>=n||path[i]!==']')return null;steps.push({{n:parseInt(ds,10)}});i+=1;continue}}if(c===']')return null;var s0=i;while(i<n&&'.|[]?"'.indexOf(path[i])<0&&' \t\n\r'.indexOf(path[i])<0)i+=1;if(i===s0)return null;steps.push({{f:path.slice(s0,i)}})}}var cur=[root];for(var k=0;k<steps.length;k++){{var stp=steps[k];var next=[];for(var m=0;m<cur.length;m++){{var v=cur[m];if(stp.f!==undefined){{if(v&&typeof v==='object'&&!Array.isArray(v)&&Object.prototype.hasOwnProperty.call(v,stp.f))next.push(v[stp.f])}}else if(stp.a){{if(Array.isArray(v)){{for(var x=0;x<v.length;x++)next.push(v[x])}}else if(v&&typeof v==='object'){{var ks=Object.keys(v);for(var x2=0;x2<ks.length;x2++)next.push(v[ks[x2]])}}}}else if(stp.n!==undefined){{if(Array.isArray(v)){{var ix=stp.n<0?v.length+stp.n:stp.n;if(ix>=0&&ix<v.length)next.push(v[ix])}}}}else if(stp.d){{var stack=[v];while(stack.length){{var t=stack.pop();next.push(t);if(Array.isArray(t)){{for(var y=t.length-1;y>=0;y--)stack.push(t[y])}}else if(t&&typeof t==='object'){{var tk=Object.keys(t);for(var y2=tk.length-1;y2>=0;y2--)stack.push(t[tk[y2]])}}}}}}}}cur=next}}return cur}}

function writeStr(p,s){{var b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p).set(b);return b.length}}
function wasmGrow(needed){{var mem=instance.exports.memory;var cur=mem.buffer.byteLength;if(needed>cur){{var pages=Math.ceil((needed-cur)/65536);mem.grow(pages)}}}}
function writeStrToWasm(s){{var b=new TextEncoder().encode(s),base=instance.exports.__heap_ptr.value,logsz=8+b.length,h=base+8;wasmGrow(base+8+logsz+8);var m=new Uint8Array(instance.exports.memory.buffer),d=new DataView(instance.exports.memory.buffer);d.setInt32(base,1,true);d.setInt32(base+4,logsz,true);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;faiLeakAlloc(h,logsz,true);return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{var n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn),d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{var l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function faiHostRetain(v){{if(instance&&instance.exports.__fai_retain)instance.exports.__fai_retain(BigInt.asIntN(64,BigInt(v)));return BigInt.asIntN(64,BigInt(v))}}
function faiHostRelease(v){{if(instance&&instance.exports.__fai_release)instance.exports.__fai_release(BigInt.asIntN(64,BigInt(v)))}}
function invokeExport(name){{var fn=instance.exports[name];if(!fn)return;var args=Array.prototype.slice.call(arguments,1);try{{return fn.apply(null,args)}}catch(e){{console.error('FAI',name,'failed',e);throw e}}}}
function rootResultText(result){{var v=wasmToJs(result);if(Array.isArray(v))return JSON.stringify(v);if(v===null||v===undefined)return'';return String(v)}}
function publishRootResult(result){{window.__FAI_ROOT_RESULT_TEXT=rootResultText(result);window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true}}
var asyncRootDone=false;
function pumpAsync(){{if(!instance||!instance.exports.__fai_poll)return 0;var status=invokeExport('__fai_poll');if(!asyncRootDone){{if(status===2){{asyncRootDone=true;if(instance.exports.__fai_task_result)publishRootResult(invokeExport('__fai_task_result',1));}}else if(status===3){{asyncRootDone=true;window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;console.error('FAI async task failed',instance.exports.__fai_task_result?invokeExport('__fai_task_result',1):null)}}}}return status}}
function startFai(){{window.__FAI_ROOT_DONE=false;window.__FAI_ROOT_RESULT_TEXT='';window.__FAI_ROOT_STARTED_AT=performance.now();window.__FAI_ROOT_FINISHED_AT=undefined;if(instance.exports._start_async){{asyncRootDone=false;invokeExport('_start_async');pumpAsync()}}else publishRootResult(invokeExport('_start'))}}
function responseHeaders(xhr){{var headers={{}};String(xhr.getAllResponseHeaders()||'').trim().split(/[\r\n]+/).forEach(function(line){{if(!line)return;var i=line.indexOf(':');if(i>0)headers[line.slice(0,i).toLowerCase()]=line.slice(i+1).trim()}});return headers}}
function httpRequest(method,url,body){{try{{var x=new XMLHttpRequest();x.open(method,url,false);if(body!==undefined)x.setRequestHeader('Content-Type','text/plain; charset=utf-8');x.send(body===undefined?null:body);return jsToWasm({{status:x.status,body:x.responseText||'',headers:responseHeaders(x)}})}}catch(e){{console.error('FAI http request failed',e);return NULL_VAL}}}}
var __faiHostOpResults={{}};
function readHostOpArgs(count,ptr){{var out=[];for(var i=0;i<count;i++)out.push(new BigInt64Array(instance.exports.memory.buffer,ptr+i*8,1)[0]);return out}}
function fetchHeaders(headers){{var out={{}};headers.forEach(function(v,k){{out[k]=v}});return out}}
function hostOpBegin(taskId,opKind,count,argsPtr,scheduler){{var args=readHostOpArgs(count,argsPtr),method={{1:'GET',2:'POST',3:'PUT',4:'PATCH',5:'DELETE'}}[opKind];function done(val){{__faiHostOpResults[taskId]=val;if(instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);scheduler()}}if(opKind===6||opKind===7){{done(NULL_VAL);return}}if(opKind===8||opKind===10){{done(jsToWasm(false));return}}if(opKind===9){{done(jsToWasm([]));return}}if(opKind===12){{done(jsToWasm(-1));return}}if(opKind>=11&&opKind<=15){{done(NULL_VAL);return}}if(!method){{done(NULL_VAL);return}}var hasBody=opKind===2||opKind===3||opKind===4,url=String(wasmToJs(args[0])||''),body=hasBody?String(wasmToJs(args[1])||''):undefined,headersArg=args[hasBody?2:1],headers=headersArg===undefined?{{}}:(wasmToJs(headersArg)||{{}}),opts={{method:method,headers:headers}};if(hasBody)opts.body=body;fetch(url,opts).then(function(r){{return r.text().then(function(t){{done(jsToWasm({{status:r.status,body:t||'',headers:fetchHeaders(r.headers)}}))}})}}).catch(function(e){{console.error('FAI host http op failed',e);done(NULL_VAL)}})}}
function hostOpResult(taskId){{var v=__faiHostOpResults[taskId];delete __faiHostOpResults[taskId];return v===undefined?NULL_VAL:v}}
var faiEventRegistry={{byName:Object.create(null),nextId:0,queue:[],draining:false}};
var __faiRpcResults={{}};
// Heap allocation ledger (plan 116 phase 5, `--check-leaks`). Armed by the
// first __fai_alloc_event from a check-leaks build (or ?fai_check_leaks=1).
// Tier 1 only in the browser: live set grouped by size, dumped on demand
// from DevTools/Playwright via window.__fai_dump_leaks().
var faiLeak={{on:new URLSearchParams(location.search).get('fai_check_leaks')==='1',map:new Map(),hostAllocs:0,hostLive:0,guestEvents:0,unknownFrees:0,bytes:0}};
function faiLeakAlloc(addr,size,host){{if(host)faiLeak.hostAllocs++;if(!host&&!faiLeak.on)faiLeak.on=true;if(!faiLeak.on)return;if(!host)faiLeak.guestEvents++;var old=faiLeak.map.get(addr);if(old!==undefined){{faiLeak.bytes-=old.size;if(old.host)faiLeak.hostLive--}}faiLeak.map.set(addr,{{size:size,host:!!host}});faiLeak.bytes+=size;if(host)faiLeak.hostLive++}}
function faiLeakFree(addr,size){{if(!faiLeak.on)return;faiLeak.guestEvents++;var s=faiLeak.map.get(addr);if(s===undefined){{faiLeak.unknownFrees++}}else{{faiLeak.map.delete(addr);faiLeak.bytes-=s.size;if(s.host)faiLeak.hostLive--}}}}
window.__fai_dump_leaks=function(){{var by={{}};faiLeak.map.forEach(function(item){{var size=item.size;by[size]=(by[size]||0)+1}});var rows=Object.keys(by).map(function(s){{return{{size:+s,count:by[s]}}}}).sort(function(a,b){{return b.size*b.count-a.size*a.count}});var live=instance&&instance.exports.__live_objects?instance.exports.__live_objects.value:null;var out='[check-leaks] live heap: '+faiLeak.map.size+' objects, '+faiLeak.bytes+' bytes ('+faiLeak.hostLive+' host-side, '+faiLeak.unknownFrees+' unknown frees'+(live===null?'':', __live_objects='+live)+')';if(faiLeak.guestEvents===0)out+='\n  no guest events — module not built with --check-leaks';rows.slice(0,40).forEach(function(r){{out+='\n  '+r.count+' × '+r.size+'B = '+(r.count*r.size)+'B'}});console.log(out);return out}};
function faiLeakTagName(tag){{return tag===0?'String':tag===1?'Array':tag===2?'Tuple':tag===3?'Dict':tag===4?'Closure':tag===5?'Module':tag===6?'NativeFn':tag===7?'Instance':tag===8?'Cell':'tag'+tag}}
function faiLeakDescribe(addr,item){{var out={{addr:addr,size:item.size,host:!!item.host}};if(!instance||!instance.exports.memory||item.host)return out;try{{var mem=instance.exports.memory.buffer,dv=new DataView(mem),u8=new Uint8Array(mem),tag=dv.getInt32(addr,true);out.tag=tag;out.kind=faiLeakTagName(tag);if(tag===0){{var len=dv.getInt32(addr+4,true);out.len=len;out.text=new TextDecoder().decode(u8.slice(addr+8,addr+8+Math.min(len,80)))}}else if(tag===1||tag===2){{out.count=dv.getInt32(addr+4,true)}}else if(tag===3||tag===7){{out.count=dv.getInt32(addr+4,true);var keys=[];for(var i=0;i<Math.min(out.count,8);i++){{var kv=new BigInt64Array(mem,addr+8+i*16,1)[0],ka=Number(BigInt.asUintN(64,kv)&0x0000FFFFFFFFFFFFn);if(ka>0&&dv.getInt32(ka,true)===0){{var kl=dv.getInt32(ka+4,true);keys.push(new TextDecoder().decode(u8.slice(ka+8,ka+8+Math.min(kl,40))))}}}}out.keys=keys}}else if(tag===4){{out.table=dv.getInt32(addr+4,true);out.upvalues=dv.getInt32(addr+8,true);out.frameSize=dv.getInt32(addr+12,true)}}else if(tag===8){{out.value='0x'+new BigInt64Array(mem,addr+8,1)[0].toString(16)}}}}catch(e){{out.error=String(e)}}return out}}
window.__fai_leak_snapshot=function(limit){{limit=limit||200;var byKind={{}},bySize={{}},items=[];faiLeak.map.forEach(function(item,addr){{var d=faiLeakDescribe(addr,item);var kind=d.kind||(item.host?'Host':'Unknown');byKind[kind]=(byKind[kind]||0)+1;bySize[item.size]=(bySize[item.size]||0)+1;if(items.length<limit)items.push(d)}});items.sort(function(a,b){{return b.size-a.size}});return{{count:faiLeak.map.size,bytes:faiLeak.bytes,hostLive:faiLeak.hostLive,unknownFrees:faiLeak.unknownFrees,byKind:byKind,bySize:bySize,items:items}}}};
var faiOwnership={{on:new URLSearchParams(location.search).get('fai_ownership_check')==='1',events:[],credits:new Map(),unmatched:[],freed:[],sites:Object.create(null),history:new Map(),sawLifecycle:false}};
var faiOwnershipOps={{1:'retain',2:'release',3:'transfer',4:'borrow',5:'store',6:'overwrite',7:'cleanup',8:'return',9:'discard',10:'call_argument'}};
function faiOwnershipReadLeb(bytes,pos){{var result=0,shift=0,b;do{{b=bytes[pos++];result|=(b&0x7f)<<shift;shift+=7}}while(b&0x80);return{{value:result,pos:pos}}}}
function faiInstallOwnershipSitesFromWasm(buffer){{try{{var bytes=new Uint8Array(buffer),pos=8,dec=new TextDecoder();while(pos<bytes.length){{var id=bytes[pos++],len=faiOwnershipReadLeb(bytes,pos);pos=len.pos;var end=pos+len.value;if(id===0){{var n=faiOwnershipReadLeb(bytes,pos),name=dec.decode(bytes.slice(n.pos,n.pos+n.value));if(name==='fai-dbg'){{var meta=JSON.parse(dec.decode(bytes.slice(n.pos+n.value,end)));(meta.ownership_sites||[]).forEach(function(s){{faiOwnership.sites[s.id]=s}});return}}}}pos=end}}}}catch(e){{debugLog('FAI ownership site metadata unavailable',e)}}}}
function faiOwnershipAddr(value){{var u=BigInt.asUintN(64,value);return (u&OBJ_MASK)===OBJ_MASK?Number(u&0x0000FFFFFFFFFFFFn):0}}
function faiOwnershipDelta(op){{return op===1||op===3?1:(op===2||op===7||op===9?-1:0)}}
function faiOwnershipAllowsUntracked(op){{return op===2||op===9}}
function faiOwnershipAlloc(addr){{if(!faiOwnership.on)return;faiOwnership.sawLifecycle=true;faiOwnership.credits.delete(addr);faiOwnership.history.delete(addr)}}
function faiOwnershipFree(addr){{if(!faiOwnership.on)return;faiOwnership.sawLifecycle=true;faiOwnership.credits.delete(addr);faiOwnership.history.delete(addr)}}
function faiOwnershipSiteLabel(site){{if(site===0)return'unknown ownership site';var s=faiOwnership.sites[site];if(!s)return'ownership site '+site;var loc=s.file&&s.line?(' ('+s.file+':'+s.line+')'):(s.line?(' (line '+s.line+')'):'');return s.helper+':'+s.op+':'+s.reason+loc}}
function faiOwnershipAuxLabel(aux){{var kind=aux>>16,detail=aux&0xffff;if(kind===0&&detail===0)return'none';if(kind===1)return'ClosureCapture:'+detail;if(kind===2)return'HostArgument:'+detail;if(kind===3)return'AsyncFrameSlot:'+detail;return String(aux)}}
function faiOwnershipFormatEvent(e){{return(faiOwnershipOps[e.op]||('op='+e.op))+' '+e.label+' aux='+e.auxLabel+' value='+e.value}}
function faiOwnershipGroupLabel(e){{return(faiOwnershipOps[e.op]||('op='+e.op))+' '+e.label+' aux='+e.auxLabel}}
function faiOwnershipGroupSummary(a){{var m=new Map();function add(k,addr){{var g=m.get(k);if(!g){{g={{key:k,count:0,addr:addr}};m.set(k,g)}}g.count++}}a.imbalances.forEach(function(i){{var h=i.history||[],last=h[h.length-1],label=last?faiOwnershipGroupLabel(last):'unknown ownership site';add('live helper credits '+(i.credits>0?'+':'')+i.credits+' at '+label,i.addr)}});a.unmatched.forEach(function(e){{add('unmatched '+(faiOwnershipOps[e.op]||('op='+e.op))+' at '+faiOwnershipGroupLabel(e),e.addr)}});return Array.from(m.values()).sort(function(a,b){{return b.count-a.count||a.key.localeCompare(b.key)}})}}
function faiOwnershipEvent(op,site,value,aux){{if(!faiOwnership.on)return;var addr=faiOwnershipAddr(value),delta=faiOwnershipDelta(op),event={{op:op,opName:faiOwnershipOps[op]||String(op),site:site,label:faiOwnershipSiteLabel(site),value:'0x'+BigInt.asUintN(64,value).toString(16),addr:addr?('0x'+addr.toString(16)):'',aux:aux,auxLabel:faiOwnershipAuxLabel(aux)}};if(addr){{var hist=faiOwnership.history.get(addr)||[];hist.push(event);if(hist.length>16)hist.shift();faiOwnership.history.set(addr,hist)}}if(addr&&delta){{var cur=faiOwnership.credits.get(addr)||0;if(delta>0){{faiOwnership.credits.set(addr,cur+delta)}}else if(cur>0){{faiOwnership.credits.set(addr,cur+delta)}}else if(!faiOwnershipAllowsUntracked(op)){{faiOwnership.unmatched.push(event)}}}}faiOwnership.events.push(event)}}
window.__fai_assert_ownership=function(){{var bad=faiOwnership.freed.slice();if(!faiOwnership.sawLifecycle)faiOwnership.credits.forEach(function(v,k){{if(v!==0)bad.push({{addr:'0x'+k.toString(16),credits:v,history:(faiOwnership.history.get(k)||[]).slice(),freed:false}})}});return{{ok:bad.length===0&&faiOwnership.unmatched.length===0,eventCount:faiOwnership.events.length,imbalances:bad,unmatched:faiOwnership.unmatched.slice(),sites:faiOwnership.sites}}}};
window.__fai_dump_ownership=function(){{var a=window.__fai_assert_ownership(),count=a.imbalances.length+a.unmatched.length,out='[ownership-check] '+a.eventCount+' event(s), '+count+' object(s) with helper imbalance',groups=faiOwnershipGroupSummary(a);if(groups.length){{out+='\n  groups:';groups.slice(0,8).forEach(function(g){{out+='\n    '+g.count+' x '+g.key+(g.addr?' (sample '+g.addr+')':'')}})}}a.imbalances.slice(0,8).forEach(function(i){{out+='\n  '+i.addr+': '+(i.freed?'freed with ':'')+'helper credits '+(i.credits>0?'+':'')+i.credits;i.history.forEach(function(e){{out+='\n    '+faiOwnershipFormatEvent(e)}})}});a.unmatched.slice(0,8-a.imbalances.length).forEach(function(e){{out+='\n  unmatched '+faiOwnershipFormatEvent(e)}});faiOwnership.events.slice(-40).forEach(function(e){{out+='\n  '+faiOwnershipFormatEvent(e)}});console.log(out);return out}};
function faiBuildEvent(name,dataVal){{var base=instance.exports.__heap_ptr.value,cap=16,logsz=8+cap*16,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,2,true);faiLeakAlloc(addr,logsz,true);faiOwnershipAlloc(addr);var kn=writeStrToWasm('name'),vn=writeStrToWasm(name),kd=writeStrToWasm('data'),data=faiHostRetain(dataVal);m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,addr+8,4);bi[0]=kn;bi[1]=vn;bi[2]=kd;bi[3]=data;return OBJ_MASK|BigInt(addr)}}
function faiBuildSubscription(id,name){{var base=instance.exports.__heap_ptr.value,cap=16,logsz=8+cap*16,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,2,true);faiLeakAlloc(addr,logsz,true);faiOwnershipAlloc(addr);var ki=writeStrToWasm('id'),kn=writeStrToWasm('name'),vn=writeStrToWasm(name),iv=INT_MASK|BigInt.asUintN(32,BigInt(id));m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,addr+8,4);bi[0]=ki;bi[1]=iv;bi[2]=kn;bi[3]=vn;return OBJ_MASK|BigInt(addr)}}
function faiReadSubscription(subVal){{var n=BigInt.asIntN(64,BigInt(subVal));var u=BigInt.asUintN(64,n);if((u&OBJ_MASK)!==OBJ_MASK)return null;var a=Number(u&0x0000FFFFFFFFFFFFn),dv=new DataView(instance.exports.memory.buffer);if(dv.getInt32(a,true)!==3)return null;var cnt=dv.getInt32(a+4,true),id=null,name=null;for(var i=0;i<cnt;i++){{var ea=a+8+i*16,bi=new BigInt64Array(instance.exports.memory.buffer,ea,2),k=readNanBoxedStr(bi[0]),v=BigInt.asUintN(64,bi[1]);if(k==='id')id=Number(BigInt.asIntN(32,v&0xFFFFFFFFn));else if(k==='name')name=readNanBoxedStr(bi[1])}}if(id===null||name===null)return null;return{{id:id,name:name}}}}
// Single-flight scheduler turn. Every async wakeup — an event closure, an RPC
// completion, a timer — funnels through here. A closure queued while a turn is
// already running (e.g. a signal-change `rerender()` emitted from inside a
// running handler) is appended and drained by that same turn, never started as a
// nested `__fai_drive_closure`/`__fai_poll` (a re-entrant poll reassigns
// `g_current` and corrupts the task table). Drives queued closures, then pumps
// the scheduler; repeats while the pump's resumed tasks queue more closures.
var __faiInScheduler=false,__faiClosureQueue=[];
function faiServiceScheduler(){{if(__faiInScheduler)return;__faiInScheduler=true;try{{var guard=0;do{{while(__faiClosureQueue.length){{var q=__faiClosureQueue.shift();try{{instance.exports.__fai_drive_closure(q[0],q[1])}}catch(e){{console.error('FAI async closure failed',e)}}finally{{faiHostRelease(q[1])}}}}pumpAsync()}}while(__faiClosureQueue.length&&guard++<100000)}}finally{{__faiInScheduler=false}}}}
function faiInvokeClosure(closureVal,arg){{var u=BigInt.asUintN(64,BigInt(closureVal));if((u&OBJ_MASK)!==OBJ_MASK)return NULL_VAL;var a=Number(u&0x0000FFFFFFFFFFFFn);if(a+16>instance.exports.memory.buffer.byteLength)return NULL_VAL;var dv=new DataView(instance.exports.memory.buffer);if(dv.getInt32(a,true)!==4)return NULL_VAL;var tidx=dv.getInt32(a+4,true),frameSize=dv.getInt32(a+12,true),envAddr=a+16;if(frameSize>0&&instance.exports.__fai_drive_closure){{__faiClosureQueue.push([BigInt.asIntN(64,BigInt(closureVal)),faiHostRetain(arg)]);faiServiceScheduler();return NULL_VAL}}if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(!tbl)return NULL_VAL;try{{return tbl.get(tidx)(BigInt.asIntN(64,BigInt(arg)))}}catch(e){{console.error('FAI event closure failed',e);return NULL_VAL}}}}
function faiEventEmit(name,dataVal){{var list=faiEventRegistry.byName[name];if(!list||list.length===0)return;var snap=list.slice(),kept=[],removed=[];for(var i=0;i<list.length;i++){{if(list[i].once)removed.push(list[i]);else kept.push(list[i])}}faiEventRegistry.byName[name]=kept;var ev=faiBuildEvent(name,dataVal);try{{for(var i=0;i<snap.length;i++)faiInvokeClosure(snap[i].closureVal,ev)}}finally{{faiHostRelease(ev);for(var i=0;i<removed.length;i++)faiHostRelease(removed[i].closureVal)}}}}
function faiEmitHostEvent(name,dataVal){{try{{faiEventEmit(name,dataVal)}}finally{{faiHostRelease(dataVal)}}}}
function handleEvent(id){{faiEmitHostEvent('view:click',jsToWasm({{id:id}}))}}
function handleInputEvent(id,value){{faiEmitHostEvent('view:input',jsToWasm({{id:id,value:value}}))}}
function handleSubmitEvent(id){{faiEmitHostEvent('view:submit',jsToWasm({{id:id}}))}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}while(op.childNodes.length>nc.length)op.removeChild(op.lastChild)}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
function wireEvents(){{document.querySelectorAll('[data-fai-click]').forEach(function(el){{var h=el.getAttribute('data-fai-click');el.onclick=function(){{invokeExport(h);startFai()}}}})}}
var env={{
  print:function(p,l){{var text=readStr(p,l);debugLog('FAI print',text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file:function(){{return -1}},write_file:function(){{return -1}},now_ms:function(){{return Date.now()}},random:function(){{return Math.random()}},sleep_ms:function(){{throw new Error('FAI legacy sleep_ms is disabled; sleep() must lower through the async scheduler')}},host_set_timer:function(taskId,ms){{setTimeout(function(){{if(instance&&instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);faiServiceScheduler()}},Math.max(0,ms|0))}},host_op_begin:function(taskId,opKind,count,argsPtr){{hostOpBegin(taskId,opKind,count,argsPtr,faiServiceScheduler)}},host_op_result:function(taskId){{return hostOpResult(taskId)}},
  call_ffi:function(){{return 0x7FFC000100000000n}},run_all:function(){{throw new Error('FAI legacy run_all is disabled; all() must lower through the async scheduler')}},
  spawn:function(closureVal){{var cv=closureVal;setTimeout(function(){{var n=BigInt(cv);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return;var tag=dv.getInt32(a,true);if(tag!==4)return;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}if(typeof faiServiceScheduler==='function')faiServiceScheduler()}},0);return 0x7FFC000200000000n}},
  http_post:function(a,b,c,d,e){{try{{var x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html:function(p,l){{morphDom(app,readStr(p,l),false);wireEvents()}},
  set_html_at:function(a,b,p,l){{var selector=readStr(a,b),html=readStr(p,l);var root=document.querySelector(selector);if(!root&&selector.charAt(0)==='#'){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root)}}if(!root)return;morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse:function(p,l){{try{{return jsToWasm(JSON.parse(readStr(p,l)))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify:function(v){{try{{return writeStrToWasm(JSON.stringify(wasmToJs(v)))}}catch(e){{return writeStrToWasm('null')}}}},
  json_query:function(p,l,qp,ql){{try{{var root=JSON.parse(readStr(p,l));var m=faiJsonQueryEval(root,readStr(qp,ql));if(m===null)return QNAN|TAG_NULL;return jsToWasm(m)}}catch(e){{return QNAN|TAG_NULL}}}},
  json_query_page:function(p,l,qp,ql,off,lim){{try{{var root=JSON.parse(readStr(p,l));var m=faiJsonQueryEval(root,readStr(qp,ql));if(m===null)return QNAN|TAG_NULL;var total=m.length;var s=Math.max(0,off|0);if(s>total)s=total;var t=Math.max(0,lim|0);return jsToWasm({{total:total,items:m.slice(s,s+t)}})}}catch(e){{return QNAN|TAG_NULL}}}},
  json_format:function(p,l){{try{{return writeStrToWasm(JSON.stringify(JSON.parse(readStr(p,l)),null,2))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_minify:function(p,l){{try{{return writeStrToWasm(JSON.stringify(JSON.parse(readStr(p,l))))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_valid:function(p,l){{try{{JSON.parse(readStr(p,l));return 1}}catch(e){{return 0}}}},
  json_stringify_pretty:function(v){{try{{return writeStrToWasm(JSON.stringify(wasmToJs(v),null,2))}}catch(e){{return writeStrToWasm('null')}}}},
  crypto_available:function(){{return 0}},
  process_available:function(){{return 0}},
  remote_call:function(a,b,c,d,e,f,g,h){{var fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);var body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});function throwBack(msg){{var box=jsToWasm({{message:msg,kind:'remote'}});instance.exports.__error_flag.value=1;instance.exports.__error_value.value=BigInt.asIntN(64,BigInt(box));return NULL_VAL}}var x=new XMLHttpRequest();try{{x.open('POST','/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body)}}catch(e){{return throwBack('network error: '+(e&&e.message?e.message:'request failed'))}}if(x.status===0)return throwBack('network error: request blocked or offline');if(x.status<200||x.status>=300)return throwBack('HTTP '+x.status+(x.statusText?': '+x.statusText:''));var resp;try{{resp=JSON.parse(x.responseText)}}catch(e){{return throwBack('invalid JSON in response')}}if(resp.ok)return jsToWasm(resp.value);return throwBack(resp.error||'remote call failed')}},
  remote_begin:function(taskId,a,b,c,d,e,f,g,h){{var fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);var body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});function done(res){{__faiRpcResults[taskId]=res;if(instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);faiServiceScheduler()}}fetch('/fai/rpc',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:body}}).then(function(r){{var st=r.status;return r.text().then(function(t){{if(st<200||st>=300){{done({{err:'HTTP '+st}});return}}var resp;try{{resp=JSON.parse(t)}}catch(e){{done({{err:'invalid JSON in response'}});return}}if(resp.ok)done({{val:jsToWasm(resp.value)}});else done({{err:resp.error||'remote call failed'}})}})}}).catch(function(e){{done({{err:'network error: '+(e&&e.message?e.message:'request failed')}})}})}},
  remote_result:function(taskId){{var res=__faiRpcResults[taskId];delete __faiRpcResults[taskId];if(!res)return NULL_VAL;if(res.err!==undefined){{var box=jsToWasm({{message:res.err,kind:'remote'}});instance.exports.__error_flag.value=1;instance.exports.__error_value.value=BigInt.asIntN(64,BigInt(box));return NULL_VAL}}return res.val}},
  float_to_str:function(v,p){{var s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);var b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  get_location_path:function(){{return writeStrToWasm(window.location.pathname)}},
  push_history_state:function(p,l){{history.pushState(null,'',readStr(p,l))}},
  replace_location:function(p,l){{window.location.replace(readStr(p,l))}},
  storage_get:function(kp,kl,bp){{try{{var k=readStr(kp,kl);var v=window.localStorage.getItem(k);if(v===null)return -1;var b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_get_str:function(kp,kl){{try{{var k=readStr(kp,kl);var v=window.localStorage.getItem(k);if(v===null)return NULL_VAL;return writeStrToWasm(v)}}catch(e){{return NULL_VAL}}}},
  file_read_str:function(){{return NULL_VAL}},
  storage_set:function(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove:function(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear:function(){{try{{window.localStorage.clear()}}catch(e){{}}}},
  env_get:function(){{return NULL_VAL}},
  env_load:function(){{return 0}},
  secrets_get:function(){{return NULL_VAL}},
  secrets_has:function(){{return 0}},
  secrets_available:function(){{return 0}},
  secrets_reveal:function(){{return NULL_VAL}},
  secrets_bearer:function(){{return NULL_VAL}},
  secrets_basic:function(){{return NULL_VAL}},
  secrets_header:function(){{return NULL_VAL}},
  secrets_refresh:function(){{return 0}},
  event_on:function(np,nl,cv){{var name=readStr(np,nl);var id=++faiEventRegistry.nextId;if(!faiEventRegistry.byName[name])faiEventRegistry.byName[name]=[];faiEventRegistry.byName[name].push({{id:id,closureVal:faiHostRetain(cv),once:false}});return faiBuildSubscription(id,name)}},
  event_once:function(np,nl,cv){{var name=readStr(np,nl);var id=++faiEventRegistry.nextId;if(!faiEventRegistry.byName[name])faiEventRegistry.byName[name]=[];faiEventRegistry.byName[name].push({{id:id,closureVal:faiHostRetain(cv),once:true}});return faiBuildSubscription(id,name)}},
  event_off:function(sv){{var sub=faiReadSubscription(sv);if(!sub)return 0;var list=faiEventRegistry.byName[sub.name];if(!list)return 0;var kept=[],removed=0;for(var i=0;i<list.length;i++){{if(list[i].id===sub.id){{faiHostRelease(list[i].closureVal);removed++}}else kept.push(list[i])}}faiEventRegistry.byName[sub.name]=kept;return removed?1:0}},
  event_emit:function(np,nl,dv){{faiEventEmit(readStr(np,nl),dv)}},
  event_subscribers:function(np,nl){{var list=faiEventRegistry.byName[readStr(np,nl)];return list?list.length:0}},
  event_clear:function(np,nl){{var name=readStr(np,nl),list=faiEventRegistry.byName[name]||[];for(var i=0;i<list.length;i++)faiHostRelease(list[i].closureVal);delete faiEventRegistry.byName[name]}},
  event_clear_all:function(){{Object.keys(faiEventRegistry.byName).forEach(function(k){{var list=faiEventRegistry.byName[k]||[];for(var i=0;i<list.length;i++)faiHostRelease(list[i].closureVal)}});for(var i=0;i<faiEventRegistry.queue.length;i++)faiHostRelease(faiEventRegistry.queue[i].dataVal);faiEventRegistry.byName=Object.create(null);faiEventRegistry.nextId=0;faiEventRegistry.queue=[];faiEventRegistry.draining=false}},
  event_emit_deferred:function(np,nl,dv){{faiEventRegistry.queue.push({{name:readStr(np,nl),dataVal:faiHostRetain(dv)}})}},
  event_drain:function(){{if(faiEventRegistry.draining)return;faiEventRegistry.draining=true;try{{while(faiEventRegistry.queue.length>0){{var ev=faiEventRegistry.queue.shift();try{{faiEventEmit(ev.name,ev.dataVal)}}finally{{faiHostRelease(ev.dataVal)}}}}}}finally{{faiEventRegistry.draining=false}}}},
  event_queue_len:function(){{return faiEventRegistry.queue.length}},
  file_exists:function(){{return 0}},
  http_request_get:function(p,l){{return httpRequest('GET',readStr(p,l))}},
  http_request_post:function(up,ul,bp,bl){{return httpRequest('POST',readStr(up,ul),readStr(bp,bl))}},
  http_request_put:function(up,ul,bp,bl){{return httpRequest('PUT',readStr(up,ul),readStr(bp,bl))}},
  http_request_patch:function(up,ul,bp,bl){{return httpRequest('PATCH',readStr(up,ul),readStr(bp,bl))}},
  http_request_delete:function(p,l){{return httpRequest('DELETE',readStr(p,l))}},
  net_available:function(){{return 0}},
  ffi_available:function(){{return 0}},
  log_info:function(p,l){{console.info(readStr(p,l))}},
  log_warn:function(p,l){{console.warn(readStr(p,l))}},
  log_error:function(p,l){{console.error(readStr(p,l))}},
  path_join:function(a,b,c,d){{var left=readStr(a,b).replace(/\/+$/,''),right=readStr(c,d).replace(/^\/+/,'');return writeStrToWasm(left+'/'+right)}},
  path_basename:function(p,l){{var s=readStr(p,l).replace(/\/+$/,'');var i=s.lastIndexOf('/');return writeStrToWasm(i>=0?s.slice(i+1):s)}},
  path_dirname:function(p,l){{var s=readStr(p,l).replace(/\/+$/,'');var i=s.lastIndexOf('/');return writeStrToWasm(i>0?s.slice(0,i):'.')}},
  path_extname:function(p,l){{var s=readStr(p,l),base=s.slice(s.lastIndexOf('/')+1),i=base.lastIndexOf('.');return writeStrToWasm(i>0?base.slice(i):'')}},
  html_escape:function(p,l){{return writeStrToWasm(readStr(p,l).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;'))}},
  file_list:function(){{return jsToWasm([])}},
  json_require_string:function(v,kp,kl){{var obj=wasmToJs(v),key=readStr(kp,kl);return writeStrToWasm(obj&&typeof obj[key]==='string'?obj[key]:'')}},
  array_map:function(arr){{return arr}},
  array_filter:function(arr){{return arr}},
  array_find:function(){{return NULL_VAL}},
  array_is_any:function(){{return QNAN|TAG_BOOL}},
  array_is_all:function(){{return QNAN|TAG_BOOL|1n}},
  tcp_listen:function(){{return 0}},
  tcp_accept:function(){{return NULL_VAL}},
  tcp_connect:function(){{return 0}},
  tcp_read:function(){{return NULL_VAL}},
  tcp_read_line:function(){{return NULL_VAL}},
  tcp_write:function(){{return -1}},
  tcp_close:function(){{}},
  tcp_address:function(){{return writeStrToWasm('')}},
  udp_bind:function(){{return 0}},
  udp_send:function(){{return -1}},
  udp_receive:function(){{return NULL_VAL}},
  udp_broadcast:function(){{}},
  cli_read_line:function(){{return NULL_VAL}},
  cli_write:function(p,l){{output.style.display='block';output.textContent+=readStr(p,l)}},
  cli_write_line:function(p,l){{output.style.display='block';output.textContent+=readStr(p,l)+'\n'}},
  cli_clear:function(){{output.textContent=''}},
  cli_move_to:function(){{}},
  __fai_alloc_event:function(addr,size){{faiLeakAlloc(addr>>>0,size>>>0,false);faiOwnershipAlloc(addr>>>0)}},
  __fai_free_event:function(addr,size){{faiLeakFree(addr>>>0,size>>>0);faiOwnershipFree(addr>>>0)}},
  __fai_ownership_event:function(op,site,value,aux){{faiOwnershipEvent(op|0,site|0,BigInt.asIntN(64,BigInt(value)),aux|0)}},
  __fai_debug_function_call:function(){{}},
  __fai_set_trap_msg:function(p,l){{var m=readStr(p,l);window.__FAI_TRAP_MSG=m;console.error('FAI trap:',m)}},
  __fai_trap_report:function(code,a,b){{
    // Plan 116: structured trap reason, mirrored from the native host
    // (wasm_runner/host/io.rs::format_trap_report). Logged before the
    // guest executes `unreachable`, so the reason survives the trap.
    function describeVal(v){{try{{var s=readNanBoxedStr(v);if(s)return 'String "'+s.slice(0,40)+'"';var j=wasmToJs(v);return j===null?'null':(typeof j==='object'?JSON.stringify(j).slice(0,80):String(j))}}catch(e){{return '<value 0x'+BigInt.asUintN(64,BigInt(v)).toString(16)+'>'}}}}
    function addrOf(v){{return '0x'+(BigInt.asUintN(64,BigInt(v))&0x0000FFFFFFFFFFFFn).toString(16)}}
    var msg;
    switch(code){{
      case 1: msg='rc-check: retain of freed object at '+addrOf(a); break;
      case 2: msg='rc-check: release of freed object at '+addrOf(a); break;
      case 3: msg='rc-check: over-release (rc '+b+') of '+describeVal(a)+' at '+addrOf(a); break;
      case 4: msg='out of memory: failed to grow linear memory ('+a+' bytes requested, heap needs 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 5: msg='async task table full ('+a+' of '+b+' slots used)'; break;
      case 6: msg='force-unwrap (`!`) of null'; break;
      case 7: msg='uncaught error: '+describeVal(a); break;
      case 8: msg='scheduler stall: poll resumed '+a+' tasks without quiescing (livelock; task t'+b+' was about to run again)'; break;
      case 9: msg='rc-check: corrupt free-list node 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' (heap_ptr 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 10: msg='rc-check: freed block at 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' was written through a stale pointer while on the free list (tag word now 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 11: msg='rc-check: double free of block at 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' (block size '+b+')'; break;
      case 12: msg='checked: index store out of bounds — xs['+a+'] = ... on an array of '+b+' elements'; break;
      case 13: msg='dict grow: implausible capacity '+a+' (size word 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+') — dictionary.set on a non-dict/stale/mis-typed pointer'; break;
      case 14: msg='alloc-guard: single allocation of '+a+' bytes ('+b+' block) exceeds 256 MB — runaway allocation'; break;
      default: msg='trap report (code '+code+', a=0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+', b=0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')';
    }}
    window.__FAI_TRAP_MSG=msg;console.error('FAI trap:',msg);
  }}
}};
fetch('/{}').then(function(r){{return r.arrayBuffer()}}).then(function(b){{faiInstallOwnershipSitesFromWasm(b);return WebAssembly.instantiate(b,{{env:env}})}}).then(function(r){{
  instance=r.instance;window.__fai_dbg=r.instance;window.__fai_live_objects=function(){{if(!instance||!instance.exports.__live_objects)return null;return instance.exports.__live_objects.value+faiLeak.hostAllocs}};startFai();
  window.addEventListener('popstate',function(){{if(instance&&instance.exports.setPathFromPlatform)instance.exports.setPathFromPlatform(writeStrToWasm(window.location.pathname))}});
}}).catch(function(e){{app.innerHTML='<p style="color:red;padding:20px">Error: '+e.message+'</p>'}});
"#,
        wasm_filename
    )
}
