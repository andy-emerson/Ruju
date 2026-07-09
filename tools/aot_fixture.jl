# AOT thin-slice fixture producer (design/research/research-aot-backend.md §7
# step 1; issue #11). Runs under the pinned Julia (tools/fetch-pinned-julia.sh),
# whose sysimage compiler *is* the pinned Compiler/ — the inferred+optimized
# IRCode serialized here is upstream's own output, version-locked to the
# reference by construction.
#
#   tools/pinned-julia/bin/julia tools/aot_fixture.jl > aotc/fixtures/f_sumsq.json
#
# Format `ruju-aotc-fixture-1`: a JSON transcription of IRCode — blocks
# (statement ranges + preds/succs), statements (phi / intrinsic call /
# gotoifnot / goto / return / nothing), per-statement types, and the
# (argtypes, rettype) of the specialization. Unsupported statement or operand
# kinds fail loudly: the thin slice's vocabulary is deliberately closed.

function f(n); acc = 0; i = 1; while i <= n; acc += i*i; i += 1; end; acc; end

const SOURCE = "function f(n); acc = 0; i = 1; while i <= n; acc += i*i; i += 1; end; acc; end"

jstr(s) = "\"" * escape_string(string(s)) * "\""

function operand(x)
    if x isa Core.SSAValue
        return """{"k":"ssa","id":$(x.id)}"""
    elseif x isa Core.Argument
        return """{"k":"arg","n":$(x.n)}"""
    elseif x isa Int64
        return """{"k":"const","t":"Int64","v":$(jstr(x))}"""
    elseif x isa Bool
        return """{"k":"const","t":"Bool","v":$(jstr(x))}"""
    elseif x === nothing
        return """{"k":"nothing"}"""
    end
    error("unsupported operand kind: $(typeof(x)) = $(repr(x))")
end

function statement(s)
    if s === nothing
        return """{"k":"nothing"}"""
    elseif s isa Core.PhiNode
        edges = join(Int.(s.edges), ",")
        vals = join((operand(s.values[i]) for i in eachindex(s.values)), ",")
        all(i -> isassigned(s.values, i), eachindex(s.values)) ||
            error("phi with undef value: $(repr(s))")
        return """{"k":"phi","edges":[$edges],"values":[$vals]}"""
    elseif s isa Core.GotoIfNot
        return """{"k":"gotoifnot","cond":$(operand(s.cond)),"dest":$(s.dest)}"""
    elseif s isa Core.GotoNode
        return """{"k":"goto","dest":$(s.label)}"""
    elseif s isa Core.ReturnNode
        isdefined(s, :val) || error("unreachable ReturnNode")
        return """{"k":"return","val":$(operand(s.val))}"""
    elseif s isa Expr && s.head === :call
        callee = s.args[1]
        callee isa GlobalRef || error("non-GlobalRef callee: $(repr(callee))")
        fval = getglobal(callee.mod, callee.name)
        fval isa Core.IntrinsicFunction ||
            error("non-intrinsic callee: $(repr(callee)) :: $(typeof(fval))")
        args = join((operand(a) for a in s.args[2:end]), ",")
        return """{"k":"call","f":$(jstr(callee.name)),"args":[$args]}"""
    end
    error("unsupported statement kind: $(typeof(s)) = $(repr(s))")
end

function emit(io, func, argtypes)
    (ir, rt) = Base.code_ircode(func, argtypes)[1]
    println(io, "{")
    println(io, """  "format": "ruju-aotc-fixture-1",""")
    println(io, """  "julia": $(jstr(Base.VERSION)),""")
    println(io, """  "producer": "Base.code_ircode under the pinned Julia (tools/fetch-pinned-julia.sh)",""")
    println(io, """  "name": $(jstr(nameof(func))),""")
    println(io, """  "source": $(jstr(SOURCE)),""")
    println(io, """  "argtypes": [$(join(map(jstr, argtypes), ","))],""")
    println(io, """  "rettype": $(jstr(rt)),""")
    blocks = map(ir.cfg.blocks) do b
        """{"first":$(first(b.stmts)),"last":$(last(b.stmts)),"preds":[$(join(Int.(b.preds), ","))],"succs":[$(join(Int.(b.succs), ","))]}"""
    end
    println(io, """  "blocks": [\n    $(join(blocks, ",\n    "))\n  ],""")
    stmts = map(1:length(ir.stmts)) do i
        inst = ir.stmts[i]
        """{"type":$(jstr(inst[:type])),"stmt":$(statement(inst[:stmt]))}"""
    end
    println(io, """  "stmts": [\n    $(join(stmts, ",\n    "))\n  ]""")
    println(io, "}")
end

emit(stdout, f, (Int64,))
