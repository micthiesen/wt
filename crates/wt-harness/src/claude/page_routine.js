function(msg, probeOnly){
    let root = this._reactInternals || this._reactInternalFiber;
    if (!root) return JSON.stringify({ok:false, err:"no react root on app instance"});
    while (root.return) root = root.return;
    // 1) the prompt fiber: onSubmit + a prompt-only semantic prop set. Claude 2.1.260
    //    replaced messagesRef/commands/onAgentSubmit with draft/transcript/scope.
    let prompt=null; const seen=new Set();
    (function v(f){ if(!f||seen.has(f)||prompt)return; seen.add(f);
      let p; try{ p=f.memoizedProps; }catch(e){ p=null; }
      if(p && typeof p==="object" && typeof p.onSubmit==="function" && ((("draft"in p)&&("transcript"in p)&&("scope"in p))||("messagesRef"in p)||("commands"in p)||("onAgentSubmit"in p))) { prompt=f; return; }
      v(f.child); v(f.sibling); v(f.alternate);
    })(root);
    if(!prompt) return JSON.stringify({ok:false, err:"prompt fiber not found"});
    // 2) the controlled text input, searched ONLY within the prompt's own subtree, so a
    //    modal/permission text box elsewhere in the tree can't be mistaken for the draft.
    let input=null; const seen2=new Set();
    (function v(f){ if(!f||seen2.has(f)||input)return; seen2.add(f);
      let p; try{ p=f.memoizedProps; }catch(e){ p=null; }
      if(p && typeof p==="object" && typeof p.onChange==="function" && typeof p.value==="string") { input=f; return; }
      v(f.child); v(f.sibling); v(f.alternate);
    })(prompt.child);
    if(probeOnly){ const pip = input ? input.memoizedProps : null; return JSON.stringify({ok:true, foundPrompt:true, foundInput: !!input, foundCaret: !!(pip && typeof pip.cursorOffset==="number" && typeof pip.onChangeCursorOffset==="function")}); }
    const ip = input ? input.memoizedProps : null;
    const draft = ip ? (ip.value || "") : "";
    const onChange = ip ? ip.onChange : null;
    // Caret position rides on the same fiber as a sibling prop pair; capture it so the
    // restored draft lands where the user left it instead of collapsing to offset 0.
    const cursor = ip && typeof ip.cursorOffset === "number" ? ip.cursorOffset : null;
    const onChangeCursor = ip && typeof ip.onChangeCursorOffset === "function" ? ip.onChangeCursorOffset : null;
    const noop = new Proxy(function(){}, { get:()=>(()=>{}), apply:()=>{} });
    try { prompt.memoizedProps.onSubmit(msg, noop, false, {}); }
    catch(e){ return JSON.stringify({ok:false, err:"onSubmit threw: "+String(e&&e.message||e)}); }
    // Restore value then caret (twice, to ride out the post-submit clear). The caret is
    // set AFTER value in the same callback so its write wins, and clamped to the draft
    // length so it survives even if a value-change effect re-clamps it -- those two, not
    // update batching, are what keep the caret off 0 (Ink's renderer doesn't batch a
    // setTimeout the way react-dom would, so don't reorder these expecting it to).
    function restore(){
      try{ onChange(draft); }catch(e){}
      if (onChangeCursor && cursor!=null) { try{ onChangeCursor(Math.min(cursor, draft.length)); }catch(e){} }
    }
    if (draft && onChange) { setTimeout(restore,50); setTimeout(restore,160); }
    return JSON.stringify({ok:true, submitted:true, draftLen:draft.length, cursor:cursor});
  }
