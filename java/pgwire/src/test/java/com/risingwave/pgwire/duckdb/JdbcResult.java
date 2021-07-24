package com.risingwave.pgwire.duckdb;

import com.risingwave.pgwire.database.PgResult;
import java.sql.ResultSet;
import java.util.Locale;
import org.jetbrains.annotations.NotNull;

public class JdbcResult implements PgResult {
  private final String stmtType;
  private int rowsCnt;
  private ResultSet res;

  private static String parseStatementType(String sql) {
    return sql.trim().split(" ", 2)[0].toUpperCase(Locale.ENGLISH);
  }

  JdbcResult(String sql, ResultSet res) {
    this.res = res;
    // Since jdbc doesn't provide API to get statement type, we parse
    // the first word from sql as the statement type.
    this.stmtType = parseStatementType(sql);
  }

  JdbcResult(String sql, int effectedRowsCnt) {
    this.rowsCnt = effectedRowsCnt;
    this.stmtType = parseStatementType(sql);
  }

  @Override
  public String getStatementType() {
    return this.stmtType;
  }

  @NotNull
  public PgIter createIterator() {
    assert isQuery();
    return new JdbcIter(this.res);
  }

  @Override
  public int getEffectedRowsCnt() {
    assert !isQuery();
    return this.rowsCnt;
  }

  @Override
  public boolean isQuery() {
    return res != null;
  }
}
