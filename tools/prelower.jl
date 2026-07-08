#!/usr/bin/env julia
# Serialize the pinned Julia's lowering of a source file into the
# RUJU_LOWERED line format (runtime/src/loader.rs consumes it) — the
# producing half of M2's build-time pre-lowering (design/strategy.md, D1).
#
# Run under the pinned Julia only (tools/fetch-pinned-julia.sh); the format
# is pin-versioned data, regenerated whenever the pin advances. Unsupported
# lowered forms fail loudly with the offending form — the corpus is bounded
# by the runtime's vocabulary, never silently truncated.
#
# Adaptations (recorded in design/implementation.md): GlobalRef module
# qualifiers drop (single module: Main); method-body slots are emitted
# pre-shifted past `#self#` (referencing `#self#` is an error until dispatch
# passes the callee); LineNumberNode constants serialize as `nothing`.

const FORMAT_VERSION = 1

fail(msg) = (println(stderr, "prelower: ", msg); exit(1))

# --- operands ----------------------------------------------------------------

# `shift`: how many leading slots are dropped (1 for toplevel thunks — slots
# become 0-based; 2 inside method bodies — `#self#` is dropped too).
function op(x, shift::Int)::String
    if x isa Core.SSAValue
        return "ssa:$(x.id - 1)"
    elseif x isa Core.SlotNumber
        n = x.id - shift
        n < 0 && fail("method body references #self# (unsupported until dispatch passes the callee)")
        return "slot:$n"
    elseif x isa GlobalRef
        return "global:$(x.name)"
    elseif x isa QuoteNode
        return constop(x.value)
    else
        return constop(x)
    end
end

function constop(v)::String
    if v isa Bool               # before Integer: Bool <: Integer
        return "bool:$(v ? 1 : 0)"
    elseif v isa Int64
        return "int:$v"
    elseif v isa Float64
        return "f64:$(string(reinterpret(UInt64, v), base = 16))"
    elseif v isa Nothing
        return "nothing"
    elseif v isa Symbol
        return "sym:$v"
    elseif v isa LineNumberNode
        return "nothing"
    elseif v isa Module
        return "module"
    elseif v isa Function       # builtin function literals (isa, svec, ...)
        return "global:$(nameof(v))"
    elseif v isa Type
        return "global:$(nameof(v))"
    else
        fail("unsupported constant $(repr(v)) :: $(typeof(v))")
    end
end

# --- statements --------------------------------------------------------------

function emit_stmt(out::Vector{String}, st, shift::Int)
    if st isa Core.GotoNode
        push!(out, "goto $(st.label - 1)")
    elseif st isa Core.GotoIfNot
        push!(out, "gotoifnot $(op(st.cond, shift)) $(st.dest - 1)")
    elseif st isa Core.ReturnNode
        isdefined(st, :val) || fail("unreachable ReturnNode")
        push!(out, "return $(op(st.val, shift))")
    elseif st isa Core.EnterNode
        isdefined(st, :scope) && st.scope !== nothing &&
            fail("scoped EnterNode (unsupported)")
        push!(out, "enter $(st.catch_dest - 1)")
    elseif st isa Expr
        emit_expr(out, st, shift)
    else
        # A bare value form as a statement (the eval_body default arm).
        push!(out, "value $(op(st, shift))")
    end
end

function emit_expr(out::Vector{String}, ex::Expr, shift::Int)
    h = ex.head
    if h === :call
        push!(out, "call " * join((op(a, shift) for a in ex.args), " "))
    elseif h === :(=)
        lhs = ex.args[1]
        lhs isa Core.SlotNumber || fail("assignment to $(repr(lhs)) (expected a slot)")
        slot = lhs.id - shift
        slot < 0 && fail("assignment to #self#")
        rhs = ex.args[2]
        if rhs isa Expr && rhs.head === :call
            push!(out, "assigncall $slot " * join((op(a, shift) for a in rhs.args), " "))
        elseif rhs isa Expr && rhs.head === :the_exception
            push!(out, "assigncaught $slot")
        elseif rhs isa Expr
            fail("assignment rhs $(repr(rhs.head)) (unsupported)")
        else
            push!(out, "assign $slot $(op(rhs, shift))")
        end
    elseif h === :leave
        push!(out, "leave $(count(a -> a !== nothing, ex.args))")
    elseif h === :pop_exception
        push!(out, "pop_exception $(op(ex.args[1], shift))")
    elseif h === :the_exception
        push!(out, "the_exception")
    elseif h === :latestworld
        push!(out, "latestworld")
    elseif h === :method && length(ex.args) == 1
        name = ex.args[1]
        name isa GlobalRef && (name = name.name)
        name isa Symbol || fail("method declaration of $(repr(name))")
        push!(out, "method1 $name")
    elseif h === :method && length(ex.args) == 3
        ci = ex.args[3]
        ci isa Core.CodeInfo || fail("3-arg :method without inline CodeInfo")
        inner = emit_body(ci, 2)  # drop #self#
        length(ci.slotnames) >= 1 && ci.slotnames[1] === Symbol("#self#") ||
            fail(":method body without leading #self# slot")
        push!(out, "method3 $(op(ex.args[1], shift)) $(op(ex.args[2], shift)) " *
                   "$(length(ci.slotnames) - 1) $(length(inner))")
        append!(out, inner)
    else
        fail("unsupported lowered form Expr(:$(h), ...)")
    end
end

function emit_body(ci::Core.CodeInfo, shift::Int)::Vector{String}
    out = String[]
    for st in ci.code
        emit_stmt(out, st, shift)
    end
    out
end

# `thunk` headers carry length(ci.code) — the count of *top-level*
# statements. Nested method bodies add lines but not top-level statements;
# the loader is count-driven per level, so the interleaving parses exactly.
function main()
    length(ARGS) == 1 || fail("usage: prelower.jl <file.jl>")
    src = read(ARGS[1], String)
    exs = Meta.parseall(src)
    chunks = String[]
    for e in exs.args
        e isa LineNumberNode && continue
        lowered = Meta.lower(Main, e)
        if lowered isa Expr && lowered.head === :thunk
            ci = lowered.args[1]
            body = emit_body(ci, 1)
            push!(chunks, "thunk $(length(ci.slotnames)) $(length(ci.code))")
            append!(chunks, body)
        elseif lowered isa Expr && lowered.head === :error
            fail("lowering error: $(lowered.args[1])")
        elseif lowered isa Symbol
            push!(chunks, "thunk 0 2", "value global:$lowered", "return ssa:0")
        else
            push!(chunks, "thunk 0 2", "value $(constop(lowered))", "return ssa:0")
        end
    end
    println("RUJU_LOWERED $FORMAT_VERSION $(Base.GIT_VERSION_INFO.commit[1:7])")
    foreach(println, chunks)
end

main()
