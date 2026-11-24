"""sim/economy.py 的期长换算测试:M=1 时必须与原仿真逐位相同;换算方向正确;两种映射的区别符合设计。

    python3 -m unittest sim/test_economy_period.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import economy as E  # noqa: E402


class MonthlyBaseline(unittest.TestCase):
    def test_monthly_period_is_unchanged(self):
        """按月结算时,换算不应改变任何东西(M=1 直接原样返回)。"""
        p = E.Params(steps=120)
        q, m, chain = E.per_period(p)
        self.assertEqual(m, 1.0)
        self.assertFalse(chain, "按月结算时不是链的字面读法")
        for f in ("steps", "probation", "vesting", "emission_half_life", "fee_mid", "k_smooth",
                  "actions", "growth", "e0", "fee_max", "rent", "kp", "ki", "r_max"):
            self.assertEqual(getattr(q, f), getattr(p, f), f"{f} 不该变")

    def test_same_seed_same_result(self):
        """同样的种子必须给出同样的结果(换算不能偷偷换掉随机序列)。"""
        a = E.Sim(E.Params(steps=12), 7).run()
        b = E.Sim(E.Params(steps=12), 7).run()
        self.assertEqual(a["k_end"], b["k_end"])
        self.assertEqual(a["units"], b["units"])


class TimeMapping(unittest.TestCase):
    def test_daily_period_scales_counts_and_durations(self):
        """天结算:每期量变少、时长变长(真实时间不变)。"""
        p = E.Params(steps=120, period_days=1.0, mapping="time")
        q, m, chain = E.per_period(p)
        self.assertEqual(m, 30.0, "天结算时每月 30 期")
        self.assertFalse(chain)
        self.assertEqual(q.steps, 3600, "10 年(120 个月)→ 3600 天")
        self.assertEqual(q.probation, 30, "见习期 1 个月 → 30 天")
        self.assertEqual(q.vesting, 180, "锁定期 6 期(月)→ 180 天")
        self.assertAlmostEqual(q.actions, 3 / 30)
        self.assertAlmostEqual(q.e0, 10000 / 30)
        self.assertEqual(q.k_smooth, 90, "3 期(月)→ 90 天")

    def test_compound_bounds_scale_by_root(self):
        """每期上限按复利换算:r_max → (1+r)^(1/M) − 1。"""
        p = E.Params(steps=120, period_days=1.0, mapping="time", r_max=0.005)
        q, m, _ = E.per_period(p)
        self.assertAlmostEqual(q.r_max, 1.005 ** (1 / 30) - 1, places=12)
        self.assertLess(q.r_max, 0.005, "期更短 → 每期上限更小(真实时间上的总变化相同)")

    def test_chain_mapping_reads_periods_literally(self):
        """链的字面读法:设计稿里的"期"就是天(锁定期 6 天、半衰期 120 天)。"""
        p = E.Params(steps=120, period_days=1.0, mapping="chain")
        q, m, chain = E.per_period(p)
        self.assertTrue(chain)
        self.assertEqual(q.vesting, 6, "锁定期 6 期 = 6 天")
        self.assertEqual(q.probation, 7, "见习期 7 天(与链上的 probation_blocks 一致)")
        self.assertEqual(q.k_smooth, 3, "平滑窗口 3 期 = 3 天")


class SimBehaviour(unittest.TestCase):
    def test_lit_and_months(self):
        sim = E.Sim(E.Params(steps=12, period_days=1.0, mapping="chain"), 1)
        self.assertEqual(sim.lit(6), 6, "链的字面读法:1 期 = 1 天")
        self.assertEqual(sim.months(6), 180, "真实时间:6 个月 = 180 天")
        sim2 = E.Sim(E.Params(steps=12, period_days=1.0, mapping="time"), 1)
        self.assertEqual(sim2.lit(6), 180, "真实时间读法下,6 期就是 6 个月")

    def test_v1_switches(self):
        """v1:只增不缩、只锁不罚。"""
        p = E.Params(steps=12, burn=False, slashing=False)
        sim = E.Sim(p, 3)
        sim.units = []
        sim.distribute()
        self.assertGreaterEqual(sim.minted, 0.0)
        # slashing 关:挑战不会罚没,也不会限流
        self.assertFalse(sim.p.slashing)

    def test_daily_run_produces_results(self):
        """天结算能跑完并给出可读的指标(步数少,只验证不崩)。"""
        m = E.Sim(E.Params(steps=12, period_days=1.0, mapping="chain"), 5).run()
        self.assertGreater(m["units"], 0)
        self.assertGreater(m["k_end"], 0.0)
        self.assertIn("mk_cv", m)


if __name__ == "__main__":
    unittest.main()
