# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

> select * from csv_upgrade_with_header
id  value   mz_line_no
----------------------
1   person  1

> select * from csv_upgrade_with_header_alias
al  sia       mz_line_no
------------------------
1   person    1

> select * from csv_upgrade_no_header
column1  column2  mz_line_no
----------------------------
id       value    1
1        person   2

> select * from csv_upgrade_no_header_alias
al  sia     mz_line_no
----------------------
id  value   1
1   person  2
